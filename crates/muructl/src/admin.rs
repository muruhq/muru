//! The commands that run or talk to the daemon (design specification
//! sections 10.4, 13.2 and 13.3, appendix A): `daemon run`, `daemon
//! start`, `daemon stop`, `daemon status`, `up`, `deploy`, `app list`,
//! `release list`, `release wait`, `rollback`, `drain`, `status`,
//! `events`, `logs`, `logs reopen`, `apply`, `deployment export` and `config
//! diff`.

use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use clap::{Args, Subcommand};
use muru_common::worker::WorkerName;
use muru_controller::client;
use muru_controller::locations::{Locations, Options};
use muru_proto::admin::v1 as pb;
use tokio_stream::StreamExt;

/// Where a running instance is found, as section 13.8 lets a command line
/// say: what every command that talks to one takes.
#[derive(Args, Clone, Default)]
pub struct InstanceArgs {
    /// The runtime directory: the admin socket and the release run directories.
    #[arg(long, global = true, value_name = "DIR")]
    run_dir: Option<PathBuf>,

    /// Use the system locations (/etc/muru, /var/lib/muru and so on)
    /// whoever runs the command.
    #[arg(long, global = true)]
    system: bool,

    /// The instance to reach: a named instance, user or system for the
    /// default daemons, or a running instance by the tag, the start of
    /// the ID, the label or the application muructl instance list shows
    /// (default: MURU_INSTANCE).
    #[arg(long, global = true, value_name = "INSTANCE")]
    instance: Option<String>,
}

/// Check an instance's name as the commands that start a daemon take it:
/// a named instance's, or `user` or `system`, since the daemon's
/// directories follow from it.
fn parse_instance_name(text: &str) -> Result<String, String> {
    muru_controller::locations::check_name(text).map_err(|error| {
        format!(
            "{error}; this command starts a daemon whose directories follow from the name, so \
             it takes a name, not the tag, ID, label or application of a running instance"
        )
    })?;
    Ok(text.to_owned())
}

/// Whether `name` is an instance reached by its name even when it is not
/// running: a default daemon, or a named instance that is defined.
pub fn is_defined(name: &str) -> bool {
    if muru_controller::locations::check_name(name).is_err() {
        return false;
    }

    muru_controller::locations::RESERVED_NAMES.contains(&name)
        || [false, true]
            .map(muru_controller::locations::caller_config_area)
            .iter()
            .any(|area| muru_controller::catalogue::find(area, name).is_some())
}

/// `MURU_INSTANCE` as it is set, whatever it names.
fn instance_variable() -> Option<String> {
    std::env::var(INSTANCE_VARIABLE)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The variable a person sets to choose the instance a command reaches,
/// as `--instance` does. Muru passes the instance a process belongs to
/// down in others, `MURU_INSTANCE_ID` and `MURU_INSTANCE_TAG`, never in
/// this one.
pub const INSTANCE_VARIABLE: &str = "MURU_INSTANCE";

/// The instance `MURU_INSTANCE` names, when the value is a name.
pub fn instance_from_environment() -> Option<String> {
    instance_variable().filter(|value| muru_controller::locations::check_name(value).is_ok())
}

/// The running instance this process belongs to, by the
/// `MURU_INSTANCE_ID` every process of an instance carries (issue 868),
/// for a `muructl` command a web application, a project's command or
/// anything else of the instance's runs. A run or a serve is passed over
/// for a command only a daemon answers, which goes on to find a daemon
/// as it would anywhere else. None outside an instance, or when the
/// instance has gone.
pub fn own_instance() -> Option<muru_controller::registry::Record> {
    let id: muru_controller::registry::InstanceId =
        std::env::var(muru_controller::instance::ID_VARIABLE)
            .ok()?
            .trim()
            .parse()
            .ok()?;
    let record = running_instances()
        .into_iter()
        .find(|record| record.id == id)?;
    let daemon_only = RUNNING.get().is_some_and(|running| running.daemon_only);

    (!daemon_only || record.kind == muru_controller::registry::Kind::Daemon).then_some(record)
}

impl InstanceArgs {
    /// The named instance the options or the environment give, which the
    /// locations follow from: one that is defined, or a default daemon.
    /// Any other value selects a running instance, as [`select_running`]
    /// decides, and has no locations of its own.
    pub fn instance(&self) -> Option<String> {
        match &self.instance {
            Some(value) => is_defined(value).then(|| value.clone()),
            None => instance_from_environment().filter(|name| is_defined(name)),
        }
    }

    /// Resolve the locations for these options and the environment.
    pub fn resolve(&self) -> Locations {
        Locations::resolve(&Options {
            run_dir: self.run_dir.clone(),
            system: self.system,
            instance: self.instance(),
            ..Options::default()
        })
    }

    /// The same options, for a muructl command run on this one's behalf.
    pub fn as_arguments(&self) -> Vec<String> {
        let mut arguments = Vec::new();

        if let Some(name) = &self.instance {
            arguments.push("--instance".to_owned());
            arguments.push(name.clone());
        }

        if let Some(path) = &self.run_dir {
            arguments.push("--run-dir".to_owned());
            arguments.push(path.display().to_string());
        }

        if self.system {
            arguments.push("--system".to_owned());
        }

        arguments
    }
}

/// The daemon's own files, as section 13.8 lets a command line say: taken
/// by the commands that start or are the daemon and by those that read
/// its files, and by no other, since a command that only talks to a
/// running daemon reads none of them.
#[derive(Args, Clone, Default)]
pub struct DaemonFileArgs {
    /// The daemon configuration file.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// The state directory: the state store and durable files.
    #[arg(long, global = true, value_name = "DIR")]
    state_dir: Option<PathBuf>,

    /// The log directory.
    #[arg(long, global = true, value_name = "DIR")]
    log_dir: Option<PathBuf>,

    /// The cache directory.
    #[arg(long, global = true, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
}

impl DaemonFileArgs {
    /// The state directory an option names, if one does.
    pub(crate) fn state_dir(&self) -> Option<&Path> {
        self.state_dir.as_deref()
    }

    /// The log directory an option names, if one does.
    pub(crate) fn log_dir(&self) -> Option<&Path> {
        self.log_dir.as_deref()
    }

    /// The cache directory an option names, if one does.
    pub(crate) fn cache_dir(&self) -> Option<&Path> {
        self.cache_dir.as_deref()
    }

    /// The daemon's configuration file an option names, if one does.
    pub(crate) fn config(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    /// The same options, for a daemon started in the background.
    pub fn as_arguments(&self) -> Vec<String> {
        let mut arguments = Vec::new();

        for (flag, value) in [
            ("--config", &self.config),
            ("--state-dir", &self.state_dir),
            ("--log-dir", &self.log_dir),
            ("--cache-dir", &self.cache_dir),
        ] {
            if let Some(path) = value {
                arguments.push(flag.to_owned());
                arguments.push(path.display().to_string());
            }
        }

        arguments
    }
}

/// Where the daemon keeps everything: what the commands that start or
/// are the daemon take. Its `--instance` is a name, never a running
/// instance's tag or ID, since the daemon's directories follow from it.
#[derive(Args, Clone, Default)]
pub struct LocationArgs {
    /// The runtime directory: the admin socket and the release run directories.
    #[arg(long, global = true, value_name = "DIR")]
    run_dir: Option<PathBuf>,

    /// Use the system locations (/etc/muru, /var/lib/muru and so on)
    /// whoever runs the command.
    #[arg(long, global = true)]
    system: bool,

    /// A named instance, whose locations all follow from its name (default:
    /// MURU_INSTANCE); user and system name the default daemons.
    #[arg(long, global = true, value_name = "NAME", value_parser = parse_instance_name)]
    instance: Option<String>,

    #[command(flatten)]
    files: DaemonFileArgs,
}

impl LocationArgs {
    /// Resolve the locations for these options and the environment. The
    /// daemon's configuration variable under the name it had before is
    /// refused, since a daemon that passed it over would start with no
    /// configuration and say nothing.
    pub fn resolve(&self) -> anyhow::Result<Locations> {
        muru_controller::locations::check_renamed().map_err(anyhow::Error::msg)?;
        let instance = self.instance.clone().or_else(instance_from_environment);

        // A name with every location given by a flag would decide nothing.

        if self.instance.is_some()
            && self.files.config.is_some()
            && self.files.state_dir.is_some()
            && self.run_dir.is_some()
            && self.files.log_dir.is_some()
        {
            anyhow::bail!(
                "--instance decides the configuration file and the state, run and log \
                 directories, and every one is given by a flag; leave out --instance or one of \
                 the flags"
            );
        }

        Ok(Locations::resolve(&Options {
            config: self.files.config.clone(),
            state_dir: self.files.state_dir.clone(),
            run_dir: self.run_dir.clone(),
            log_dir: self.files.log_dir.clone(),
            cache_dir: self.files.cache_dir.clone(),
            system: self.system,
            instance,
        }))
    }

    /// The options that find the instance, for a command that reaches it
    /// once it runs.
    pub fn instance_args(&self) -> InstanceArgs {
        InstanceArgs {
            run_dir: self.run_dir.clone(),
            system: self.system,
            instance: self.instance.clone(),
        }
    }

    /// The same options, for a daemon started in the background.
    fn as_arguments(&self) -> Vec<String> {
        let mut arguments = self.files.as_arguments();
        arguments.extend(self.instance_args().as_arguments());
        arguments
    }
}

/// Whether a command prints the daemon's answer as JSON instead of text.
#[derive(Args, Clone, Copy, Default)]
pub struct JsonArgs {
    /// Print the answer as JSON, with the admin API's field names.
    #[arg(long)]
    pub json: bool,
}

/// How to reach a daemon or a development run: its admin socket.
#[derive(Args, Clone, Default)]
pub struct SocketArgs {
    /// The admin socket (default: MURU_SOCKET, else the instance
    /// MURU_INSTANCE names, else for a command run inside an instance that
    /// instance, else the one muru.local.toml names, else a development run
    /// whose project is the current directory or that was started from
    /// it, else the user daemon's socket in the runtime directory, else
    /// the one development run listening).
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,

    #[command(flatten)]
    locations: InstanceArgs,
}

/// The variable that names an admin socket.
const SOCKET_VARIABLE: &str = "MURU_SOCKET";

/// Where a name that chose the instance came from (section 13.8, issue
/// 811), for saying so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChosenBy {
    /// `--instance`.
    Flag,
    /// `MURU_INSTANCE`.
    Variable,
    /// The instance the process runs under, by `MURU_INSTANCE_ID`.
    Own,
    /// `instance` in a muru.local.toml.
    LocalFile(PathBuf),
}

/// What the command being run is, for choosing its instance: whether
/// only a daemon answers it, so a development run of the directory is
/// not preferred, what it changes, said when a name chose the instance,
/// and the project whose muru.local.toml is read.
#[derive(Debug, Default)]
pub struct Running {
    /// Only a daemon answers it.
    pub daemon_only: bool,
    /// What it does, when it changes something, such as `deploying shop`.
    pub change: Option<String>,
    /// The directory whose muru.local.toml is read, when not the current
    /// one, as `muructl deploy APP PATH` reads PATH's.
    pub project: Option<PathBuf>,
}

/// The command being run, set once by main.
static RUNNING: std::sync::OnceLock<Running> = std::sync::OnceLock::new();

/// The instance name the command reached, for `--json` output.
static REACHED: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Say what the command being run is, before it chooses its instance.
pub fn set_running(running: Running) {
    let _ = RUNNING.set(running);
}

/// The instance name the command reached by name, if it did.
pub fn reached_instance() -> Option<&'static str> {
    REACHED.get().map(String::as_str)
}

/// The commands that work on this machine's processes and files rather
/// than through the admin API (section 13.8, issue 811), marked here and
/// nowhere else. Each refuses an instance that is not on this machine,
/// through [`require_local`]; every other command goes through the admin
/// API and works on any endpoint.
pub const LOCAL_ONLY: [&str; 8] = [
    "open",
    "instance prune",
    "instance list --orphans and --ports",
    "daemon start",
    "daemon run",
    "the logs commands on files",
    "cache",
    "secret",
];

/// Refuse a local-only command, one of [`LOCAL_ONLY`], for an instance
/// that is not on this machine.
pub fn require_local(what: &str, name: Option<&str>) -> anyhow::Result<()> {
    debug_assert!(
        LOCAL_ONLY.contains(&what),
        "{what} is not marked local-only"
    );

    let Some(name) = name else {
        return Ok(());
    };

    let defined = [false, true]
        .map(muru_controller::locations::caller_config_area)
        .iter()
        .find_map(|area| muru_controller::catalogue::find(area, name));

    match defined {
        Some(definition) if !definition.is_local() => anyhow::bail!(
            "instance {name} is not on this machine; {what} works only on local instances"
        ),
        _ => Ok(()),
    }
}

/// The endpoint of the instance `name` names: the default daemons by
/// their locations, a named instance from the catalogue of definitions
/// and then its live registry entry. A remote definition would give its
/// endpoint here, with nothing else changing.
pub fn resolve_name(name: &str, system: bool) -> anyhow::Result<client::Endpoint> {
    if muru_controller::locations::RESERVED_NAMES.contains(&name) {
        let locations = Locations::resolve(&Options {
            system,
            instance: Some(name.to_owned()),
            ..Options::default()
        });
        return Ok(client::Endpoint::unix(locations.admin_socket()));
    }

    let areas = [false, true].map(muru_controller::locations::caller_config_area);
    let defined = areas
        .iter()
        .find_map(|area| muru_controller::catalogue::find(area, name));

    let Some(definition) = defined else {
        let none_defined = areas
            .iter()
            .all(|area| muru_controller::catalogue::read(area).0.is_empty());

        if none_defined {
            anyhow::bail!(
                "no instance {name} is defined, and no named instances are defined at all; \
                 define it with muructl instance create {name} --listen ADDRESS"
            );
        }

        anyhow::bail!(
            "no instance {name} is defined; muructl instance list shows the ones that are"
        );
    };

    let live = muru_controller::registry::readable_registries()
        .iter()
        .flat_map(|registry| muru_controller::registry::live(registry))
        .find(|record| record.name.as_deref() == Some(definition.name.as_str()));

    match live {
        Some(record) => Ok(record.admin),
        None => anyhow::bail!(
            "instance {name} is not running; start it with muructl daemon start --instance {name}"
        ),
    }
}

/// The live instances on this machine that a command may reach, from the
/// caller's registry and the system daemon's, each once.
pub fn running_instances() -> Vec<muru_controller::registry::Record> {
    let mut records: Vec<muru_controller::registry::Record> =
        muru_controller::registry::readable_registries()
            .iter()
            .flat_map(|registry| muru_controller::registry::live(registry))
            .collect();
    records.sort_by_key(|record| record.started_unix_ms);
    records.dedup_by_key(|record| record.id);
    records
}

/// The running instance `wanted` selects (section 13.8, issue 868): by
/// its name, tag, the start of its ID, its label or its application, as
/// [`muru_controller::selector`] decides, with its endpoint and the value
/// that selects it.
pub fn select_running(wanted: &str, by: &ChosenBy) -> anyhow::Result<(client::Endpoint, String)> {
    let records = running_instances();

    match muru_controller::selector::select(wanted, &records) {
        Ok(choice) => Ok((
            choice.record.admin.clone(),
            muru_controller::selector::handle(choice.record, &records),
        )),
        Err(refusal) => {
            let ambiguous = matches!(
                refusal,
                muru_controller::selector::Refusal::Ambiguous { .. }
            );

            if !ambiguous && let Some(message) = not_running(wanted) {
                anyhow::bail!(message);
            }

            let instead = match by {
                ChosenBy::Variable => format!("set {INSTANCE_VARIABLE} to one of these"),
                _ => "pass one of these to --instance".to_owned(),
            };

            anyhow::bail!(refusal.message(&instead, "muructl instance list"))
        }
    }
}

/// What to say when `wanted` selects no running instance but one that is
/// listed as no longer running, detached or stale, by the same rules: that
/// instance and why it cannot be reached, rather than a value nothing
/// knows. None when no such instance matches it alone.
fn not_running(wanted: &str) -> Option<String> {
    let registries = crate::instances::registries(false).ok()?;
    let rows: Vec<crate::instances::Row> =
        crate::instances::rows(&registries, &muru_controller::lookup::muru_processes())
            .into_iter()
            .filter(|row| {
                matches!(
                    row.state,
                    crate::instances::State::Detached | crate::instances::State::Stale
                )
            })
            .collect();
    let records: Vec<muru_controller::registry::Record> =
        rows.iter().map(|row| row.record.clone()).collect();

    let choice = muru_controller::selector::select(wanted, &records).ok()?;
    let row = rows.iter().find(|row| row.record.id == choice.record.id)?;
    let what = row.record.summary();

    let why = match (row.state, row.record.kind, row.record.name.as_deref()) {
        (crate::instances::State::Detached, _, Some(name)) => format!(
            "its daemon has gone and left its edge and hosts serving; start it again with \
             muructl daemon start --instance {name} to reach it"
        ),
        (crate::instances::State::Detached, _, None) => "its daemon has gone and left its \
             edge and hosts serving; start the daemon again on the same directories to reach \
             it"
        .to_owned(),
        _ => "its entry was left behind when it stopped, and muructl instance prune removes it"
            .to_owned(),
    };

    Some(format!(
        "{wanted} is {what}, which is not running: {why}; muructl instance list shows the \
         running instances"
    ))
}

impl SocketArgs {
    /// The socket the command line or `MURU_SOCKET` names, if either does.
    pub fn given_socket(&self) -> Option<PathBuf> {
        self.socket.clone().or_else(|| {
            std::env::var_os(SOCKET_VARIABLE)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
    }

    /// Choose the instance to reach (section 13.8, issues 811 and 868):
    /// `--socket` or `--instance`, then `MURU_SOCKET` or `MURU_INSTANCE`,
    /// then the instance this process runs under, then `instance` in
    /// muru.local.toml, then discovery. A development run of this
    /// directory still wins over the local file for a command both answer,
    /// with a note; a flag or variable is explicit and wins.
    pub fn choose(&self) -> anyhow::Result<client::Endpoint> {
        let system = self.locations.system;

        match (&self.socket, &self.locations.instance) {
            (Some(_), Some(_)) => {
                anyhow::bail!("--socket and --instance both name what to reach; give one")
            }
            (Some(socket), None) => return Ok(client::Endpoint::unix(socket)),
            (None, Some(name)) => return self.chosen_by_name(name, ChosenBy::Flag, system),
            (None, None) => {}
        }

        let variable_socket = std::env::var_os(SOCKET_VARIABLE).filter(|value| !value.is_empty());

        match (variable_socket, instance_variable()) {
            (Some(_), Some(_)) => anyhow::bail!(
                "{SOCKET_VARIABLE} and {INSTANCE_VARIABLE} are both set, and both name what to \
                 reach; unset one"
            ),
            (Some(socket), None) => return Ok(client::Endpoint::unix(PathBuf::from(socket))),
            (None, Some(name)) => return self.chosen_by_name(&name, ChosenBy::Variable, system),
            (None, None) => {}
        }

        // A command run inside an instance, by its application or one of
        // its project's commands, reaches that instance (issue 868).

        if let Some(record) = own_instance() {
            let handle = muru_controller::selector::handle(&record, &running_instances());
            self.say_change(&handle, &ChosenBy::Own);
            return Ok(record.admin);
        }

        // The checkout's own file, unless a development run of this
        // directory answers a command it can.

        let running = RUNNING.get();
        let directory = running
            .and_then(|running| running.project.clone())
            .or_else(|| std::env::current_dir().ok());

        if let Some(directory) = directory
            && let Some((file, name)) =
                muru_config::local_instance(&directory).map_err(anyhow::Error::msg)?
        {
            let daemon_only = running.is_some_and(|running| running.daemon_only);

            if !daemon_only && let Some(run) = self.run_here() {
                static NOTED: std::sync::Once = std::sync::Once::new();

                NOTED.call_once(|| {
                    muru_common::muru_eprintln!(
                        "reaching the development run of {} {}; {} names instance \
                         {name}, pass --instance {name} to reach it",
                        run.app,
                        run.on(),
                        muru_config::LOCAL_FILE
                    );
                });

                return Ok(client::Endpoint::unix(run.socket));
            }

            return self.chosen_by_name(&name, ChosenBy::LocalFile(file), system);
        }

        Ok(client::Endpoint::unix(self.socket()?))
    }

    /// The instance a flag, the variable or the checkout's muru.local.toml
    /// chooses, as the value given, without reaching it: what `muructl mcp
    /// serve` settles once at start.
    pub fn named(&self) -> anyhow::Result<Option<String>> {
        if self.socket.is_some() {
            return Ok(None);
        }

        if let Some(name) = &self.locations.instance {
            return Ok(Some(name.clone()));
        }

        if std::env::var_os(SOCKET_VARIABLE).is_some_and(|value| !value.is_empty()) {
            return Ok(None);
        }

        if let Some(value) = instance_variable() {
            return Ok(Some(value));
        }

        let directory = std::env::current_dir()?;
        Ok(muru_config::local_instance(&directory)
            .map_err(anyhow::Error::msg)?
            .map(|(_, name)| name))
    }

    /// The newest development run whose project is the current directory,
    /// or that was started from it, when one listens.
    fn run_here(&self) -> Option<crate::run_admin::RunInfo> {
        let here = std::env::current_dir()
            .and_then(|directory| directory.canonicalize())
            .ok()?;
        let runs = crate::run_admin::runs(&self.locations.resolve().run.path);

        runs.into_iter()
            .filter(|run| {
                run.project == here || run.started_from.as_deref() == Some(here.as_path())
            })
            .max_by_key(|run| run.started_unix_ms)
    }

    /// The instance `name` chooses, by its name when it is defined and
    /// else among the running instances, said when the command changes
    /// something. The local file's value is always a name.
    fn chosen_by_name(
        &self,
        name: &str,
        by: ChosenBy,
        system: bool,
    ) -> anyhow::Result<client::Endpoint> {
        let (endpoint, name) = if is_defined(name) || matches!(by, ChosenBy::LocalFile(_)) {
            (resolve_name(name, system)?, name.to_owned())
        } else {
            select_running(name, &by)?
        };
        self.say_change(&name, &by);
        Ok(endpoint)
    }

    /// Note the instance a name or the process's own instance chose, for
    /// `--json` output, and say where a change went when the command
    /// changes something.
    fn say_change(&self, name: &str, by: &ChosenBy) {
        let _ = REACHED.set(name.to_owned());

        if let Some(change) = RUNNING.get().and_then(|running| running.change.as_deref()) {
            static SAID: std::sync::Once = std::sync::Once::new();

            let from = match by {
                ChosenBy::LocalFile(_) => format!(" (from {})", muru_config::LOCAL_FILE),
                ChosenBy::Variable => format!(" (from {INSTANCE_VARIABLE})"),
                ChosenBy::Own => " (the instance this runs under)".to_owned(),
                ChosenBy::Flag => String::new(),
            };

            SAID.call_once(|| muru_common::muru_eprintln!("{change} to instance {name}{from}"));
        }
    }

    /// The same options, for a muructl command run on this one's behalf,
    /// which then reaches the same daemon.
    pub fn as_arguments(&self) -> Vec<String> {
        let mut arguments = self
            .socket
            .iter()
            .flat_map(|socket| ["--socket".to_owned(), socket.display().to_string()])
            .collect::<Vec<_>>();
        arguments.extend(self.locations.as_arguments());
        arguments
    }

    /// The runtime directory the command was given, if any.
    pub fn run_dir(&self) -> Option<&Path> {
        self.locations.run_dir.as_deref()
    }

    /// The daemon's log, for a command of the daemon's own that names
    /// it: where the instance's options and the daemon's files put it.
    pub(crate) fn daemon_log(&self, files: &DaemonFileArgs) -> PathBuf {
        Locations::resolve(&Options {
            run_dir: self.locations.run_dir.clone(),
            system: self.locations.system,
            log_dir: files.log_dir.clone(),
            state_dir: files.state_dir.clone(),
            instance: self.locations.instance(),
            ..Options::default()
        })
        .daemon_log()
    }

    /// The options that find the instance, for `muructl up`.
    pub fn location_arguments(&self) -> Vec<String> {
        self.locations.as_arguments()
    }

    /// The socket of whatever serves the project now (section 9.12): a
    /// muructl serve in the run directory, of this project when one is
    /// `named`, else a development run started from it. One that no
    /// longer answers is passed over.
    pub fn serving(&self, project: &Path, named: bool) -> Option<PathBuf> {
        let answers = |socket: &Path| std::os::unix::net::UnixStream::connect(socket).is_ok();

        let serve_dir = std::env::var_os("MURU_RUN_DIR").map_or_else(
            || PathBuf::from(crate::serve::DEFAULT_RUN_DIR),
            PathBuf::from,
        );
        let serve_socket = serve_dir.join(crate::serve::ADMIN_SOCKET);

        let serve = std::fs::read(serve_dir.join("serve.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<crate::run_admin::RunInfo>(&bytes).ok())
            .filter(|info| !named || info.project == project);

        if serve.is_some() && answers(&serve_socket) {
            return Some(serve_socket);
        }

        crate::run_admin::runs(&self.locations.resolve().run.path)
            .into_iter()
            .find(|run| run.project == project && answers(&run.socket))
            .map(|run| run.socket)
    }

    /// The socket a command would connect to as a phrase: the daemon's,
    /// a development run's, or another named by its path.
    pub fn describe(&self) -> String {
        let socket = match self.socket() {
            Ok(socket) => socket,
            Err(error) => return format!("no admin socket yet ({error})"),
        };

        let locations = self.locations.resolve();

        if socket == locations.admin_socket() {
            return format!("the daemon at {}", socket.display());
        }

        match crate::run_admin::runs(&locations.run.path)
            .iter()
            .find(|run| run.socket == socket)
        {
            Some(run) => format!("the development run of {} at {}", run.app, socket.display()),
            None => format!("the admin socket at {}", socket.display()),
        }
    }

    /// The admin endpoint to connect to (design specification section
    /// 13.8), as [`SocketArgs::choose`] decides it.
    pub fn endpoint(&self) -> anyhow::Result<client::Endpoint> {
        self.choose()
    }

    /// The socket to connect to (design specification section 13.8): as
    /// given, else a muructl serve's in its run directory, else a
    /// development run's from this project, else the daemon's when it
    /// listens, else the one run listening; several runs and no daemon
    /// are named, so one can be chosen.
    pub fn socket(&self) -> anyhow::Result<PathBuf> {
        if let Some(socket) = &self.socket {
            return Ok(socket.clone());
        }

        // A named instance's socket is where its name puts it, running or
        // not; any other value selects a running instance (issue 868).
        // The flag comes before the variables, the socket's before the
        // instance's.

        let variable_socket = std::env::var_os(SOCKET_VARIABLE)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);

        let selected = match (&self.locations.instance, variable_socket) {
            (Some(value), _) => Some((value.clone(), ChosenBy::Flag)),
            (None, Some(socket)) => return Ok(socket),
            (None, None) => instance_variable().map(|value| (value, ChosenBy::Variable)),
        };

        if let Some((value, by)) = selected {
            if self.locations.instance().is_some() {
                return Ok(self.locations.resolve().admin_socket());
            }

            let (endpoint, _) = select_running(&value, &by)?;
            return endpoint
                .socket()
                .map(Path::to_path_buf)
                .ok_or_else(|| anyhow::anyhow!("{value} is not reached through a socket"));
        }

        // Issue 868: a command run inside an instance reaches that one.

        if let Some(socket) =
            own_instance().and_then(|record| record.admin.socket().map(Path::to_path_buf))
        {
            return Ok(socket);
        }

        // Section 13.9: inside a container, the serve that runs there.

        let serve_dir = std::env::var_os("MURU_RUN_DIR").map_or_else(
            || PathBuf::from(crate::serve::DEFAULT_RUN_DIR),
            PathBuf::from,
        );
        let serve_socket = serve_dir.join(crate::serve::ADMIN_SOCKET);

        let serve_file = serve_dir.join("serve.json");

        if serve_file.is_file() && serve_socket.exists() {
            return Ok(serve_socket);
        }

        // A run directory this user may not enter is not the same as none:
        // discovery carries on past it, and comes back to it only if it
        // finds nothing else, so that `connect` can say what was denied.

        let denied = may_not_enter(&serve_file).then_some(serve_socket);

        let locations = self.locations.resolve();
        let daemon = locations.admin_socket();
        let daemon_listening = daemon.exists() && !muru_controller::client::left_behind(&daemon);
        let runs = crate::run_admin::runs(&locations.run.path);
        let here = std::env::current_dir()
            .and_then(|directory| directory.canonicalize())
            .ok();

        let records = running_instances();
        let reach = |run: &crate::run_admin::RunInfo| reach_run(&run.socket, &records);
        let discovery = discovered(
            &runs,
            here.as_deref(),
            daemon,
            daemon_listening,
            denied,
            &reach,
        )?;

        // A command may look for its socket more than once; the note that
        // the daemon was chosen over a run is said once.

        static NOTED: std::sync::Once = std::sync::Once::new();

        let note = discovery
            .note
            .clone()
            .or_else(|| chosen_among_several(&discovery.socket, &locations.run.path));

        if let Some(note) = note {
            NOTED.call_once(|| muru_common::muru_eprintln!("{note}"));
        }

        Ok(discovery.socket)
    }
}

/// What to say when discovery chose `socket` while other instances it
/// could have reached run (issue 868): which one it is, and how to choose
/// another. The candidates are the running instances under the caller's
/// run directory, as discovery itself sees them. None when there was no
/// other, or nothing running has the socket.
fn chosen_among_several(socket: &Path, run_dir: &Path) -> Option<String> {
    let records: Vec<muru_controller::registry::Record> = running_instances()
        .into_iter()
        .filter(|record| record.run_dir.starts_with(run_dir))
        .collect();

    if records.len() < 2 {
        return None;
    }

    let chosen = records
        .iter()
        .find(|record| record.admin.socket() == Some(socket))?;

    Some(format!(
        "reaching {}, instance {}, one of {} running; pass --instance to choose another, as \
         muructl instance list names them",
        chosen.describe(),
        muru_controller::selector::handle(chosen, &records),
        records.len()
    ))
}

/// Where discovery led: the socket, and what to tell the user about the
/// choice when a development run listens that was passed over for the
/// daemon.
#[derive(Debug)]
struct Discovery {
    socket: PathBuf,
    note: Option<String>,
}

impl Discovery {
    fn quiet(socket: PathBuf) -> Self {
        Self { socket, note: None }
    }
}

/// Whether looking at `path` is refused because a directory on the way
/// to it may not be entered, as opposed to the path being absent.
fn may_not_enter(path: &Path) -> bool {
    matches!(
        path.try_exists(),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied
    )
}

/// The rest of discovery (design specification section 13.8), once there
/// is no usable serve: a development run whose project is the current
/// directory, then one started from it, the newest of several, with a
/// note naming the others, the daemon's socket when it
/// listens, then the one run listening. With nothing there, the socket
/// of a run directory this user may not enter when there is one, so the
/// permission error is reported, or the daemon's. When the daemon is
/// chosen while runs listen, the note says how to reach them. `reach`
/// gives the option that reaches a run, `--instance` with what
/// `muructl instance list` shows for it; the notes name few runs and
/// point at that list for the rest (issue 885).
fn discovered(
    runs: &[crate::run_admin::RunInfo],
    here: Option<&Path>,
    daemon: PathBuf,
    daemon_listening: bool,
    denied: Option<PathBuf>,
    reach: &dyn Fn(&crate::run_admin::RunInfo) -> String,
) -> anyhow::Result<Discovery> {
    // The runs whose project is this directory, else those started from
    // it; of several, the newest (issue 714), with a note naming the
    // others and how to reach them.

    let mut mine: Vec<&crate::run_admin::RunInfo> = runs
        .iter()
        .filter(|run| here.is_some_and(|here| run.project == here))
        .collect();

    if mine.is_empty() {
        mine = runs
            .iter()
            .filter(|run| here.is_some() && run.started_from.as_deref() == here)
            .collect();
    }

    mine.sort_by_key(|run| run.started_unix_ms);

    if let Some((newest, others)) = mine.split_last() {
        let note = (!others.is_empty()).then(|| {
            let others: Vec<String> = others.iter().rev().map(|run| reach(run)).collect();

            format!(
                "{} development runs of this project are listening; reaching the newest, {} \
                 {}; {} {}",
                mine.len(),
                newest.app,
                newest.on(),
                listed(&others, muru_controller::selector::MAX_LISTED),
                if others.len() == 1 {
                    "reaches the other"
                } else {
                    "reach the others"
                }
            )
        });

        return Ok(Discovery {
            socket: newest.socket.clone(),
            note,
        });
    }

    if daemon_listening {
        let note = match runs {
            [] => None,
            [run] => Some(format!(
                "using the daemon; the development run of {} {} is also \
                 listening: {}, or the command run from {}, reaches it",
                run.app,
                run.on(),
                reach(run),
                run.project.display()
            )),
            several => Some(format!(
                "using the daemon; {} development runs are also listening: pass --instance to \
                 reach one, as muructl instance list names them",
                several.len()
            )),
        };

        return Ok(Discovery {
            socket: daemon,
            note,
        });
    }

    match runs {
        [] => Ok(Discovery::quiet(denied.unwrap_or(daemon))),
        [run] => Ok(Discovery::quiet(run.socket.clone())),
        several => {
            let mut message = format!(
                "no daemon is running and {} development runs are; choose one, or run the \
                 command from its project directory:",
                several.len()
            );

            let shown: Vec<(&crate::run_admin::RunInfo, String)> = several
                .iter()
                .take(muru_controller::selector::MAX_LISTED)
                .map(|run| (run, reach(run)))
                .collect();
            let width = shown.iter().map(|(_, how)| how.len()).max().unwrap_or(0);

            for (run, how) in &shown {
                message.push_str(&format!(
                    "\n  {how:width$}  {} {} from {}",
                    run.app,
                    run.on(),
                    run.project.display()
                ));
            }

            if several.len() > muru_controller::selector::MAX_LISTED {
                message.push_str(&format!(
                    "\n  and {} more; muructl instance list shows them all",
                    several.len() - muru_controller::selector::MAX_LISTED
                ));
            }

            anyhow::bail!(message)
        }
    }
}

/// `items` joined with commas, the first `most` of them, and how many
/// more after.
fn listed(items: &[String], most: usize) -> String {
    let mut text = items
        .iter()
        .take(most)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");

    if items.len() > most {
        text.push_str(&format!(" and {} more", items.len() - most));
    }

    text
}

/// The option that reaches a run among the running `records`:
/// `--instance` with what `muructl instance list` shows for it, or
/// `--socket` and its path when no record has its socket.
pub fn reach_run(socket: &Path, records: &[muru_controller::registry::Record]) -> String {
    records
        .iter()
        .find(|record| record.admin.socket() == Some(socket))
        .map_or_else(
            || format!("--socket {}", socket.display()),
            |record| {
                format!(
                    "--instance {}",
                    muru_controller::selector::handle(record, records)
                )
            },
        )
}

/// The daemon subcommands.
#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground.
    Run(DaemonRunArgs),

    /// Start the daemon in the background and wait until it answers.
    Start(DaemonRunArgs),

    /// Stop the daemon, leaving hosts and the edge serving for the next
    /// daemon to re-adopt; with --shutdown, drain and stop everything.
    Stop {
        /// Drain every release, stop the edge and the hosts, then exit.
        #[arg(long)]
        shutdown: bool,

        /// How long to wait for the daemon to exit.
        #[arg(long, value_name = "DURATION", default_value = "90s", value_parser = muru_config::parse_duration)]
        timeout: Duration,

        #[command(flatten)]
        socket: SocketArgs,

        /// The daemon's own files, as it was started with them.
        #[command(flatten)]
        files: DaemonFileArgs,
    },

    /// Re-read the daemon's configuration file and apply what changed:
    /// the edge's tables, the listeners, the limits, the event sinks and
    /// the budget; the reload says what is read only at start.
    Reload {
        #[command(flatten)]
        socket: SocketArgs,

        /// The daemon's own files, as it was started with them.
        #[command(flatten)]
        files: DaemonFileArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Re-execute the daemon from the installed muructl and replace the
    /// edge, without interrupting traffic: hosts and the edge keep
    /// serving and the new daemon takes them over.
    Upgrade {
        /// How long to wait for the upgraded daemon to answer with a
        /// new edge.
        #[arg(long, value_name = "DURATION", default_value = "120s", value_parser = muru_config::parse_duration)]
        timeout: Duration,

        #[command(flatten)]
        socket: SocketArgs,

        /// The daemon's own files, as it was started with them.
        #[command(flatten)]
        files: DaemonFileArgs,
    },

    /// Re-execute the daemon from the muructl it runs, so that it reads
    /// its configuration file afresh, the settings read only at start
    /// among them: hosts and the edge keep serving, and the daemon
    /// keeps its pid.
    Restart {
        /// How long to wait for the restarted daemon to be ready.
        #[arg(long, value_name = "DURATION", default_value = "120s", value_parser = muru_config::parse_duration)]
        timeout: Duration,

        #[command(flatten)]
        socket: SocketArgs,

        /// The daemon's own files, as it was started with them.
        #[command(flatten)]
        files: DaemonFileArgs,
    },

    /// Show the daemon's status, or where its files would be when none
    /// is running, exiting 3 then.
    Status {
        #[command(flatten)]
        socket: SocketArgs,

        /// The daemon's own files, as it was started with them.
        #[command(flatten)]
        files: DaemonFileArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Put the instance back to nothing, with everything of it stopped:
    /// remove its state store and what its releases left (environments,
    /// bytecode, static directories, the upload spool), after listing
    /// what goes and what stays and asking. The configuration file is
    /// kept, and so are application data and logs unless --data and
    /// --logs. Refused while a daemon, an edge or a host of it runs.
    Reset(Box<crate::daemon_reset::ResetArgs>),

    /// The daemon's own configuration file: where it is, what it holds,
    /// whether the daemon would take it, and changes that are checked,
    /// written and reloaded. What the daemon serves is not in it: that
    /// is muructl apply's.
    Config {
        #[command(subcommand)]
        command: crate::daemon_config::DaemonConfigCommand,
    },
}

/// What `daemon run` and `daemon start` take.
#[derive(Args, Clone)]
pub struct DaemonRunArgs {
    #[command(flatten)]
    pub label: crate::instances::LabelArgs,

    /// Serve plain HTTP on this address alone, instead of the [[listener]]
    /// entries of the configuration file (default 127.0.0.1:8080).
    #[arg(long, value_name = "ADDRESS")]
    listen: Option<SocketAddr>,

    /// Serve Prometheus metrics at /metrics on this address, instead of
    /// the [metrics] table's listen address (none by default).
    #[arg(long, value_name = "ADDRESS")]
    metrics: Option<SocketAddr>,

    /// Serve the dashboard and JSON gateway on this loopback
    /// address, instead of the [dashboard] table's listen address (none
    /// by default).
    #[arg(long, value_name = "ADDRESS")]
    dashboard: Option<SocketAddr>,

    /// The service user a daemon running as root starts the edge, and
    /// the applications that name no user, as; instead of the [daemon]
    /// table's user (default muru). The account must exist and must not
    /// be root.
    #[arg(long, value_name = "NAME")]
    user: Option<String>,

    /// Put the full UTC time in front of every line on standard output
    /// and standard error, for a supervisor that stamps nothing; instead
    /// of the [logs] table's timestamps. Log files always carry the time.
    #[arg(long)]
    timestamps: bool,

    /// Apply these files once the daemon is up, as muructl apply -f
    /// does, and report ready only when they are applied: a file, or a
    /// directory whose .toml files are applied together; repeatable. They
    /// are applied at every start, and to a daemon that is already
    /// running. A start whose files cannot be applied fails, leaving
    /// what was serving as it was.
    #[arg(long, value_name = "FILE or DIR")]
    apply: Vec<PathBuf>,

    /// With --apply: also remove what the files do not contain, of the
    /// deployments they name; with --prune=all, of every deployment, so
    /// the files are the whole of what the daemon serves.
    #[arg(
        long,
        requires = "apply",
        value_name = "REACH",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "files"
    )]
    prune: Option<PruneScope>,

    /// Set by a daemon that re-executed itself for an upgrade: the
    /// version it ran before.
    #[arg(long, hide = true, value_name = "VERSION")]
    upgraded_from: Option<String>,

    /// Set by a daemon that re-executed itself for a restart.
    #[arg(long, hide = true)]
    restarted: bool,

    /// Set by a daemon that re-executed itself for an upgrade: the
    /// descriptors that hold its locks on the state and run directories.
    #[arg(long, hide = true, value_name = "FDS")]
    held_locks: Option<String>,

    #[command(flatten)]
    locations: LocationArgs,
}

/// The arguments a daemon re-executes itself with on an upgrade or a
/// restart: its own, without the flags an earlier re-execution gave it.
/// The flag that says which this one is, and the descriptors that hold
/// its locks, are added when it re-executes.
fn reexec_arguments() -> Vec<std::ffi::OsString> {
    without_reexec_flags(muru_common::title::args_os().into_iter().skip(1).collect())
}

/// Arguments without the flags a re-execution adds, each with its value
/// when it takes one.
fn without_reexec_flags(mut arguments: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    for (flag, takes_value) in [
        (muru_controller::daemon::UPGRADED_FROM_FLAG, true),
        (muru_controller::daemon::RESTARTED_FLAG, false),
        (muru_controller::ownership::HELD_LOCKS_FLAG, true),
    ] {
        if let Some(at) = arguments.iter().position(|argument| argument == flag) {
            let end = (at + 1 + usize::from(takes_value)).min(arguments.len());
            arguments.drain(at..end);
        }
    }

    arguments
}

/// Run the daemon in the foreground.
pub async fn daemon_run(args: DaemonRunArgs) -> anyhow::Result<()> {
    let locations = args.locations.resolve()?;
    check_defined(&locations)?;
    require_local("daemon run", locations.instance.as_deref())?;

    // Issue 804: the daemon owns its state and run directories before it
    // does anything else, so a second daemon on either is refused before
    // it can take over the first's socket or edge.

    let held = muru_controller::ownership::parse_held(args.held_locks.as_deref().unwrap_or(""))
        .map_err(anyhow::Error::msg)?;
    let ownership = muru_controller::ownership::Ownership::take(&locations, &held)
        .map_err(anyhow::Error::msg)?;

    // Section 2.1: the daemon's process title, which names whose daemon
    // it is, as its edge's does.

    // Section 13.10: the instance's ID, kept in the state directory, and
    // its tag, which the daemon's processes carry.

    let id = muru_controller::registry::InstanceId::kept_in(
        &locations
            .state
            .path
            .join(muru_controller::daemon::INSTANCE_ID_FILE),
    )
    .with_context(|| {
        format!(
            "keeping the instance ID in {}",
            locations.state.path.display()
        )
    })?;
    let tag = locations.instance_name().unwrap_or_else(|| id.short());
    muru_controller::instance::set(id, &tag);
    muru_common::title::set(
        &muru_controller::instance::scoped_title("daemon", &locations.scope(), Some(&tag)),
        "muru-daemon",
    );

    // The file is read the same way here and on every reload, with the
    // command line's settings laid over it.

    let overrides = muru_controller::settings::Overrides {
        listen: args.listen,
        metrics: args.metrics,
        dashboard: args.dashboard,
        user: args.user.clone(),
    };
    let settings = muru_controller::settings::NodeSettings::read(
        &locations.config.path,
        &locations.state.path.join("secrets"),
        &overrides,
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    muru_common::log::show_timestamps(args.timestamps || settings.timestamps);

    // The edge writes the access log to a file in the log directory as
    // JSON lines.

    std::fs::create_dir_all(&locations.logs.path)
        .with_context(|| format!("creating {}", locations.logs.path.display()))?;
    let access_log = Some(locations.logs.path.join("access.log"));

    for (name, location) in [
        ("config", &locations.config),
        ("state", &locations.state),
        ("run", &locations.run),
        ("logs", &locations.logs),
        ("cache", &locations.cache),
    ] {
        muru_common::muru_println!("{name}: {} ({})", location.path.display(), location.rule);
    }

    muru_controller::daemon::run(muru_controller::daemon::Config {
        locations,
        settings,
        overrides,
        access_log,
        executable: std::env::current_exe().context("finding the muructl executable")?,
        reexec_arguments: reexec_arguments(),
        upgraded_from: args.upgraded_from.clone(),
        restarted: args.restarted,
        rename_check: rename_check(),
        ownership,
        start_apply: start_apply(&args),
    })
    .await
}

/// The files `--apply` names, read here as muructl apply reads them, so
/// that their paths are taken from where each file is. Files that cannot
/// be read as one configuration are carried as the reason, which the
/// daemon reports once it has restored what its store holds.
fn start_apply(args: &DaemonRunArgs) -> Option<muru_controller::daemon::StartApply> {
    if args.apply.is_empty() {
        return None;
    }

    let paths: Vec<String> = args
        .apply
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    let documents = load_documents(&args.apply).map_err(|error| format!("{error:#}"));

    Some(muru_controller::daemon::StartApply {
        paths: paths.join(" and "),
        documents,
        prune: PruneScope::reach(args.prune),
    })
}

/// How often the daemon checks for a renamed log file: every five
/// seconds, or as `MURU_RENAME_CHECK_MS` says, which the tests set so
/// that they need not wait.
fn rename_check() -> Duration {
    std::env::var("MURU_RENAME_CHECK_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(muru_controller::daemon::RENAME_CHECK, Duration::from_millis)
}

/// Start the daemon in the background: a detached `muructl daemon run`
/// writing to the daemon log, waited for until its socket answers.
pub async fn daemon_start(args: DaemonRunArgs) -> anyhow::Result<()> {
    let locations = args.locations.resolve()?;
    locations
        .check_run_dir()
        .map_err(|error| anyhow::anyhow!(error))?;
    check_defined(&locations)?;
    require_local("daemon start", locations.instance.as_deref())?;
    let socket = locations.admin_socket();

    // A daemon answering on this socket is this one when it uses the same
    // state directory, and another daemon on the run directory when not.

    if let Ok(mut client) = client::connect(&client::Endpoint::unix(&socket)).await
        && let Ok(status) = client.get_status(pb::GetStatusRequest {}).await
    {
        let status = status.into_inner();

        if Path::new(&status.state_dir) == locations.state.path {
            println!("muru: the daemon is already running (pid {})", status.pid);

            // The files are applied to it all the same: the command means
            // running, and serving these files (ADR 0081).

            return apply_to_running(&args, &socket);
        }

        anyhow::bail!(
            "another daemon (pid {}) is using the run directory {}; its admin socket is {}",
            status.pid,
            locations.run.path.display(),
            socket.display()
        );
    }

    // Issue 808: a listener the daemon would bind that another instance
    // holds is refused before anything starts, naming that instance.

    refuse_held_addresses(&args, &locations)?;

    std::fs::create_dir_all(&locations.logs.path)
        .with_context(|| format!("creating {}", locations.logs.path.display()))?;
    std::fs::create_dir_all(&locations.run.path)
        .with_context(|| format!("creating {}", locations.run.path.display()))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(locations.daemon_log())
        .with_context(|| format!("opening {}", locations.daemon_log().display()))?;

    // Its own session, so it survives this shell and its signals.

    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("daemon").arg("run");

    if let Some(listen) = args.listen {
        command.arg("--listen").arg(listen.to_string());
    }

    if let Some(metrics) = args.metrics {
        command.arg("--metrics").arg(metrics.to_string());
    }

    if let Some(dashboard) = args.dashboard {
        command.arg("--dashboard").arg(dashboard.to_string());
    }

    if let Some(user) = &args.user {
        command.arg("--user").arg(user);
    }

    // The daemon reads the files itself, so their paths are made whole
    // here, where a relative one means something.

    for path in &args.apply {
        command
            .arg("--apply")
            .arg(std::path::absolute(path).unwrap_or_else(|_| path.clone()));
    }

    if let Some(scope) = args.prune {
        command.arg(scope.flag());
    }

    command.args(args.locations.as_arguments());
    command
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);

    // SAFETY: setsid is async-signal-safe and the closure runs in the
    // child between fork and exec, where it touches nothing else.

    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    // Where this daemon's lines start in the log, which it appends to.

    let written_before = std::fs::metadata(locations.daemon_log()).map_or(0, |found| found.len());
    let mut child = command.spawn().context("starting the daemon")?;

    // Until it answers, a daemon that has exited is reported at once,
    // with what it wrote, rather than waited for.

    let started = Instant::now();

    while started.elapsed() < Duration::from_secs(30) {
        if let Ok(mut client) = client::connect(&client::Endpoint::unix(&socket)).await
            && let Ok(status) = client.get_status(pb::GetStatusRequest {}).await
        {
            let status = status.into_inner();

            // With files to apply, started means applied: the daemon is
            // ready only then, and one whose files cannot be applied
            // exits, with why in its log.

            if !args.apply.is_empty() {
                await_start_apply(&socket, &mut child, &locations, written_before).await?;
            }

            println!(
                "muru: daemon started (pid {}), serving on {}, log {}",
                status.pid,
                status.listeners.join(" and "),
                locations.daemon_log().display()
            );

            if !args.apply.is_empty() {
                let written = daemon_log_since(&locations.daemon_log(), written_before);

                if let Some(line) = written
                    .lines()
                    .rfind(|line| line.contains("start apply of "))
                {
                    println!("muru: {}", after_stamp(line));
                }
            }

            return Ok(());
        }

        if let Some(exited) = child.try_wait().context("waiting for the daemon")? {
            // Issue 804: a daemon refused its directories says so, and
            // that is the whole of the answer.

            let written = daemon_log_since(&locations.daemon_log(), written_before);

            if let Some(refusal) = written
                .lines()
                .find(|line| muru_controller::ownership::is_refusal(line))
                .and_then(|line| line.split_once("error: "))
            {
                anyhow::bail!("{}", refusal.1);
            }

            // A start whose files could not be applied says that, and no
            // more of the log.

            if let Some(failed) = written
                .lines()
                .rfind(|line| line.contains("start apply of ") && line.contains(" failed: "))
                .and_then(|line| line.split_once("error: "))
            {
                anyhow::bail!("{}", failed.1.trim_end_matches(" (subject=daemon)"));
            }

            anyhow::bail!("the daemon exited before it answered ({exited}); {written}");
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    anyhow::bail!(
        "the daemon did not answer within 30 seconds; see {}",
        locations.daemon_log().display()
    )
}

/// A daemon's log line without the time and the `muru:` in front of it.
fn after_stamp(line: &str) -> &str {
    line.split_once("muru: ").map_or(line, |(_, rest)| rest)
}

/// Wait for a daemon started with `--apply` to finish applying: until it
/// answers that it has started up, or exits, when what its log says of
/// the apply is the error.
async fn await_start_apply(
    socket: &Path,
    child: &mut std::process::Child,
    locations: &Locations,
    written_before: u64,
) -> anyhow::Result<()> {
    loop {
        if let Some(exited) = child.try_wait().context("waiting for the daemon")? {
            let written = daemon_log_since(&locations.daemon_log(), written_before);
            let why = written
                .lines()
                .rfind(|line| line.contains("start apply of ") && line.contains(" failed: "))
                .and_then(|line| line.split_once("error: "))
                .map(|(_, why)| why.trim_end_matches(" (subject=daemon)"));

            match why {
                Some(why) => anyhow::bail!("{why}"),
                None => anyhow::bail!("the daemon exited while applying ({exited}); {written}"),
            }
        }

        // The daemon may be gone between one look and the next, so each
        // look connects again.

        let started = match client::connect(&client::Endpoint::unix(socket)).await {
            Ok(mut client) => client
                .probe(pb::ProbeRequest {
                    probe: "startup".to_owned(),
                    ..pb::ProbeRequest::default()
                })
                .await
                .map(|answer| answer.into_inner().ok)
                .unwrap_or(false),
            Err(_) => false,
        };

        if started {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Apply the `--apply` files of a start to the daemon that already
/// runs, by running muructl apply against it, and fail as it fails.
fn apply_to_running(args: &DaemonRunArgs, socket: &Path) -> anyhow::Result<()> {
    if args.apply.is_empty() {
        return Ok(());
    }

    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("apply").arg("--socket").arg(socket);

    for path in &args.apply {
        command.arg("-f").arg(path);
    }

    if let Some(scope) = args.prune {
        command.arg(scope.flag());
    }

    let status = command.status().context("running muructl apply")?;

    if !status.success() {
        anyhow::bail!("the files were not applied; the daemon runs on as it was");
    }

    Ok(())
}

/// Refuse to start a named instance with no configuration file, naming
/// the command that defines it; the default daemon starts without one.
fn check_defined(locations: &Locations) -> anyhow::Result<()> {
    let Some(name) = &locations.instance else {
        return Ok(());
    };

    if !locations.config.path.is_file() {
        anyhow::bail!(
            "instance {name} has no configuration file at {}; define it with muructl instance \
             create {name}",
            locations.config.path.display()
        );
    }

    Ok(())
}

/// Refuse to start a daemon whose listeners, dashboard or metrics address
/// a running instance of Muru's holds, naming it. The daemon itself reads
/// its file again and reports anything else wrong with it.
fn refuse_held_addresses(args: &DaemonRunArgs, locations: &Locations) -> anyhow::Result<()> {
    let overrides = muru_controller::settings::Overrides {
        listen: args.listen,
        metrics: args.metrics,
        dashboard: args.dashboard,
        user: args.user.clone(),
    };
    let Ok(settings) = muru_controller::settings::NodeSettings::read(
        &locations.config.path,
        &locations.state.path.join("secrets"),
        &overrides,
    ) else {
        return Ok(());
    };

    let addresses = settings
        .listeners
        .iter()
        .map(|(address, _)| (*address, "listener"))
        .chain(
            settings
                .dashboard
                .map(|address| (address, "dashboard address")),
        )
        .chain(settings.metrics.map(|address| (address, "metrics address")));

    let addresses: Vec<(SocketAddr, &str)> = addresses.collect();

    for (address, what) in &addresses {
        if let Some(message) = muru_controller::registry::in_use_by_instance(
            *address,
            &format!("choose another {what}"),
        ) {
            anyhow::bail!(message);
        }
    }

    // A clash with a named instance that is not running is only said.

    for (name, configured) in crate::instances::configured_elsewhere(locations) {
        if let Some((address, _)) = addresses.iter().find(|(address, _)| {
            muru_controller::registry::clashes(configured.as_str(), &address.to_string())
        }) {
            muru_common::muru_eprintln!(
                "warning: {address} is also configured for instance {name}, which is not running"
            );
        }
    }

    Ok(())
}

/// The lines a daemon wrote to its log from `offset` on, the last
/// `DAEMON_LOG_LINES` of them, as the end of a message about it.
fn daemon_log_since(path: &Path, offset: u64) -> String {
    use std::io::{Read as _, Seek as _};

    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        file.seek(std::io::SeekFrom::Start(offset))?;
        file.read_to_end(&mut bytes)
    });

    let written = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = written.lines().collect();

    if read.is_err() || lines.is_empty() {
        return format!("see {}", path.display());
    }

    let last: Vec<String> = lines[lines.len().saturating_sub(DAEMON_LOG_LINES)..]
        .iter()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("  {line}")
            }
        })
        .collect();

    format!("from {}:\n{}", path.display(), last.join("\n"))
}

/// How many of a daemon's last lines `daemon start` shows when it exits
/// before answering.
const DAEMON_LOG_LINES: usize = 20;

/// How long one exchange with a daemon that is being stopped may take
/// before it counts as not answering.
const STOP_EXCHANGE: Duration = Duration::from_secs(5);

/// Ask the daemon at `path` for its status, within `bound`. None when
/// there is no daemon there, it refused, or it did not answer in time.
pub(crate) async fn status_within(path: &Path, bound: Duration) -> Option<pb::Status> {
    let exchange = async {
        let mut client = client::connect(&client::Endpoint::unix(path)).await.ok()?;
        client
            .get_status(pb::GetStatusRequest {})
            .await
            .ok()
            .map(|status| status.into_inner())
    };

    tokio::time::timeout(bound, exchange).await.ok().flatten()
}

/// Stop the daemon through its API and wait for its process to go.
///
/// Every exchange with the daemon is bounded, so a daemon that accepts a
/// connection and then never answers, or one that lingers after it has
/// logged its exit, cannot hold the command past `timeout`.
pub async fn daemon_stop(
    socket: SocketArgs,
    files: DaemonFileArgs,
    shutdown: bool,
    timeout: Duration,
) -> anyhow::Result<()> {
    let path = socket.socket()?;
    let started = Instant::now();
    let bound = timeout.min(STOP_EXCHANGE);
    let connected =
        tokio::time::timeout(bound, client::connect(&client::Endpoint::unix(&path))).await;

    let mut client = match connected {
        Ok(client) => client?,
        Err(_) => anyhow::bail!(
            "the daemon at {} did not complete a connection within {} seconds",
            path.display(),
            bound.as_secs()
        ),
    };

    let status = tokio::time::timeout(bound, client.get_status(pb::GetStatusRequest {}))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "the daemon at {} did not answer within {} seconds",
                path.display(),
                bound.as_secs()
            )
        })??
        .into_inner();

    // The daemon may close the connection as it exits, or take its time
    // answering; whether it did exit is checked below either way.

    let _ = tokio::time::timeout(
        bound,
        client.shutdown(pb::ShutdownRequest { drain: shutdown }),
    )
    .await;

    // The daemon removes its socket as it exits; a process that has exited
    // but not been reaped would still answer kill(0), so the socket is the
    // signal to wait for.

    while started.elapsed() < timeout {
        let remaining = timeout.saturating_sub(started.elapsed());
        let answering = status_within(&path, remaining.min(bound)).await.is_some();

        if !answering && (!path.exists() || !process_alive(status.pid)) {
            if shutdown {
                println!("muru: daemon stopped; releases drained and stopped");
            } else {
                println!("muru: daemon detached; hosts and the edge keep serving");
            }
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    anyhow::bail!(
        "the daemon (pid {}) did not exit within {} seconds; see {}",
        status.pid,
        timeout.as_secs(),
        socket.daemon_log(&files).display()
    )
}

/// Re-execute the daemon from the binary it runs and wait until it is
/// ready again.
pub async fn daemon_restart(
    socket: SocketArgs,
    files: DaemonFileArgs,
    timeout: Duration,
) -> anyhow::Result<()> {
    restart_at(
        &socket.socket()?,
        &socket.daemon_log(&files),
        timeout,
        false,
    )
    .await
}

/// Restart the daemon whose admin socket is at `path`, whose log is
/// `log`, and wait until it is ready again, saying nothing of how it
/// goes when `quiet`.
pub async fn restart_at(
    path: &Path,
    log: &Path,
    timeout: Duration,
    quiet: bool,
) -> anyhow::Result<()> {
    let mut client = client::connect(&client::Endpoint::unix(path)).await?;
    let before = client
        .get_status(pb::GetStatusRequest {})
        .await?
        .into_inner();
    let response = client
        .restart_daemon(pb::RestartDaemonRequest {})
        .await
        .map_err(|status| anyhow::anyhow!("{}", status.message()))?
        .into_inner();

    if !quiet {
        println!(
            "muru: daemon (pid {}) re-executing from {}",
            before.pid, response.executable
        );
    }

    // The daemon answers again from the new image with a new start time,
    // and is ready once it has re-adopted the hosts and the edge.

    let started = Instant::now();
    let mut answered = false;

    while started.elapsed() < timeout {
        let remaining = timeout.saturating_sub(started.elapsed());
        let status = status_within(path, remaining.min(STOP_EXCHANGE)).await;

        if let Some(status) = status.filter(|now| now.started_unix_ms != before.started_unix_ms) {
            answered = true;

            if ready_within(path, remaining.min(STOP_EXCHANGE)).await {
                if !quiet {
                    println!("muru: daemon restarted (pid {})", status.pid);
                }

                return Ok(());
            }
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    if answered {
        anyhow::bail!(
            "the daemon re-executed (pid {}) but was not ready within {} seconds; see muructl \
             events",
            before.pid,
            timeout.as_secs()
        )
    }

    anyhow::bail!(
        "the daemon (pid {}) did not answer again within {} seconds; see {}",
        before.pid,
        timeout.as_secs(),
        log.display()
    )
}

/// Whether the daemon at `path` says within `bound` that it has started
/// all it restores.
async fn ready_within(path: &Path, bound: Duration) -> bool {
    let exchange = async {
        let mut client = client::connect(&client::Endpoint::unix(path)).await.ok()?;
        client
            .probe(pb::ProbeRequest {
                probe: "startup".to_owned(),
                ..pb::ProbeRequest::default()
            })
            .await
            .ok()
            .map(|answer| answer.into_inner().ok)
    };

    tokio::time::timeout(bound, exchange)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Re-execute the daemon from the installed binary and wait until it
/// answers again with the edge replaced.
pub async fn daemon_upgrade(
    socket: SocketArgs,
    files: DaemonFileArgs,
    timeout: Duration,
) -> anyhow::Result<()> {
    let path = socket.socket()?;
    let mut client = client::connect(&client::Endpoint::unix(&path)).await?;
    let before = client
        .get_status(pb::GetStatusRequest {})
        .await?
        .into_inner();

    // Section 13.8 (issue 948): a daemon that keeps its lock and records
    // in its run directory is from before they moved to the state
    // directory. This version would start without the edge and the hosts
    // that daemon runs, which would be left running with nothing
    // looking after them, so it is not asked to re-execute.

    if muru_controller::locations::earlier_daemon_owns(Path::new(&before.run_dir)) {
        anyhow::bail!(
            "the daemon (pid {}, version {}) keeps its runtime files in its run directory {}, \
             and this version of Muru keeps them in the state directory, so it cannot take \
             over that daemon's edge and releases and does not upgrade it in place; stop it and \
             its releases with muructl daemon stop --shutdown, then start it again with \
             muructl daemon start",
            before.pid,
            before.version,
            before.run_dir
        );
    }

    let response = client
        .upgrade_daemon(pb::UpgradeDaemonRequest {})
        .await
        .map_err(|status| anyhow::anyhow!("{}", status.message()))?
        .into_inner();
    println!(
        "muru: daemon (pid {}, version {}) re-executing from {} (version {})",
        before.pid, before.version, response.executable, response.version
    );

    // The daemon answers again from the new image with a new start time,
    // re-adopts the hosts, and then replaces the edge.

    let started = Instant::now();
    let mut latest = None;

    while started.elapsed() < timeout {
        let remaining = timeout.saturating_sub(started.elapsed());
        let status = status_within(&path, remaining.min(STOP_EXCHANGE)).await;

        if let Some(status) = status {
            if status.started_unix_ms != before.started_unix_ms
                && status.edge_pid != 0
                && status.edge_pid != before.edge_pid
            {
                println!(
                    "muru: daemon upgraded to version {} (pid {}); edge pid {} replaced by pid {}",
                    status.version, status.pid, before.edge_pid, status.edge_pid
                );
                return Ok(());
            }

            latest = Some(status);
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    match latest {
        Some(status) if status.started_unix_ms != before.started_unix_ms => anyhow::bail!(
            "the daemon re-executed (pid {}, version {}) but the edge was not replaced; \
             see muructl events",
            status.pid,
            status.version
        ),
        _ => anyhow::bail!(
            "the daemon (pid {}) did not answer again within {} seconds; see {}",
            before.pid,
            timeout.as_secs(),
            socket.daemon_log(&files).display()
        ),
    }
}

/// What `debug` takes: a release or an application (its current
/// release), one worker or all, and the output form.
#[derive(Args)]
pub struct DebugArgs {
    /// The release as app@release, or an application for its current
    /// release; the one application of `muructl run` or `muructl serve`
    /// when left out.
    #[arg(value_name = "APP[@RELEASE]")]
    target: Option<String>,

    /// One worker instead of every worker of the release: its index
    /// within its group, as GROUP/N outside the default group.
    #[arg(long, value_name = "[GROUP/]N")]
    worker: Option<WorkerName>,

    /// Print each worker's answer as JSON instead of text.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    socket: SocketArgs,
}

/// The debug subcommands (section 11.7).
#[derive(Subcommand)]
pub enum DebugCommand {
    /// Each worker's pid, uptime, requests served and in flight, the age
    /// of its oldest request and its memory, from the host.
    Status(DebugArgs),

    /// The Python stack of every thread in every worker, with the request
    /// each thread is serving, merged where workers show the same stack.
    Stacks(DebugArgs),

    /// The asyncio tasks of each ASGI worker's event loop, what each
    /// awaits, and the loop's lag.
    Tasks(DebugArgs),

    /// Each worker's interpreter, sys.path, loaded modules, threads,
    /// descriptors and environment: every variable's name, with values
    /// only for Muru's own, the interpreter's and the release's unless
    /// --env asks for every one; values that look like secrets are
    /// masked either way.
    Inspect(InspectArgs),

    /// The logging and warnings setup each worker has in force: its
    /// loggers with their levels, effective levels, handlers and where
    /// each level came from, the logging module's switches, and the
    /// warnings filters. Only the loggers with something set unless
    /// --all is given. Reads only.
    Logging(LoggingSetupArgs),

    /// Sample every thread's stack for a period, merged across the
    /// workers, as collapsed stacks for a flame graph or as pstats; with
    /// --requests or --path, only the requests chosen.
    Profile(ProfileArgs),

    /// Count calls and time every Python function with sys.monitoring
    /// for a period (Python 3.12 or later); refused on a busy worker
    /// unless forced.
    Trace(TraceArgs),

    /// Each worker's memory, gc state and most common object types, and
    /// with --trace the sites that allocated most over a period, or with
    /// --diff since a snapshot kept with --snapshot.
    Memory(MemoryArgs),

    /// A development run's record of one request, by the ID in its
    /// X-Request-ID header: its timing, where it was routed, its
    /// response and headers, with secret values masked, and its
    /// connection. muructl run keeps the records of its last 1000
    /// requests.
    Request(RequestRecordArgs),

    /// Run Python code in the workers and print its output and value.
    /// Allowed as it is under muructl run; under the daemon, only when
    /// its [daemon] table sets debug_exec.
    Exec(ExecArgs),

    /// The import report of a release's load: what each import cost,
    /// which entry point paid for it, and what to defer or load before
    /// the fork, as configuration to write; then each module the workers
    /// have loaded for the first time since the fork, by how many of them
    /// and what to do about it. The host's load by default, or with
    /// --worker a worker's own, when it loaded the application itself.
    /// The release needs import_report on, or the report of a load
    /// muructl check --imports prints.
    Imports(ImportsArgs),

    /// A short summary of the installation to attach to a bug report:
    /// versions, how Muru runs, each release's interface, framework and
    /// state, the serving releases' settings and recent events. Safe to
    /// post: no log lines, request data, environment or secrets, and
    /// application names, host names, paths and addresses replaced by
    /// placeholders. Works with no daemon running, on its files.
    Report(crate::diagnostics::report::ReportArgs),

    /// An archive of the whole system for a time window, to triage with:
    /// the logs and events of the window, the configuration in force, and
    /// what the workers are doing. It holds production data and is not
    /// for posting; it is written readable by its owner alone. Works with
    /// no daemon running, on its files, and records what it could not
    /// collect in its manifest.
    Bundle(crate::diagnostics::bundle::BundleArgs),

    /// Open a pdb session in a worker: on the thread stopped at
    /// breakpoint(), else on a thread of its own. Each line read is a pdb
    /// command, and the end of input lets the code go on. Without
    /// --worker, the first worker with a breakpoint waiting. Allowed as it
    /// is under muructl run; under the daemon, only when its [daemon]
    /// table sets debug_exec.
    Attach(AttachArgs),
}

/// `muructl debug request`.
#[derive(Args)]
pub struct RequestRecordArgs {
    /// The request's ID, as X-Request-ID, Server-Timing and the access
    /// log give it.
    #[arg(value_name = "ID")]
    request_id: String,

    #[command(flatten)]
    output: JsonArgs,

    #[command(flatten)]
    socket: SocketArgs,
}

/// `muructl debug inspect`.
#[derive(Args)]
pub struct InspectArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// Show the value of every environment variable, masked where it
    /// looks like a secret, not only Muru's, the interpreter's and the
    /// release's.
    #[arg(long)]
    env: bool,
}

/// `muructl debug logging`.
#[derive(Args)]
pub struct LoggingSetupArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// List every logger that exists, not only those with a level, a
    /// handler or a flag of their own.
    #[arg(long)]
    all: bool,
}

/// `muructl debug attach`.
#[derive(Args)]
pub struct AttachArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// Carry the Debug Adapter Protocol for an IDE instead of a pdb
    /// session, to debugpy in the worker, which the release's
    /// environment must have.
    #[arg(long)]
    dap: bool,

    /// Where the IDE connects with --dap.
    #[arg(long, value_name = "ADDRESS", default_value = "127.0.0.1:5678")]
    listen: SocketAddr,
}

/// `muructl debug profile`.
#[derive(Args)]
pub struct ProfileArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// How long to sample.
    #[arg(long = "for", value_name = "DURATION", default_value = "10s", value_parser = muru_config::parse_duration)]
    duration: Duration,

    /// How often to sample.
    #[arg(long, value_name = "DURATION", default_value = "10ms", value_parser = muru_config::parse_duration)]
    interval: Duration,

    /// Sample only the threads serving requests whose path starts with
    /// this.
    #[arg(long, value_name = "PREFIX")]
    path: Option<String>,

    /// End once this many of the requests sampled have finished, within
    /// --for.
    #[arg(long, value_name = "N")]
    requests: Option<u32>,

    /// Collapsed stacks for a flame graph, or pstats, which needs
    /// --output.
    #[arg(long, value_enum, default_value_t = ProfileFormat::Collapsed)]
    format: ProfileFormat,

    /// Write the profile here rather than to standard output.
    #[arg(long, short, value_name = "FILE")]
    output: Option<PathBuf>,
}

/// How `muructl debug profile` writes the samples.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProfileFormat {
    /// One line per stack, root first, and its count.
    Collapsed,
    /// Python's pstats, from the samples.
    Pstats,
}

/// `muructl debug trace`.
#[derive(Args)]
pub struct TraceArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// How long to trace.
    #[arg(long = "for", value_name = "DURATION", default_value = "10s", value_parser = muru_config::parse_duration)]
    duration: Duration,

    /// Trace a worker serving requests in more than half its slots.
    #[arg(long)]
    force: bool,

    /// How many functions to show, by time.
    #[arg(long, value_name = "N", default_value_t = 30)]
    top: u32,
}

/// `muructl debug memory`.
#[derive(Args)]
pub struct MemoryArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// Trace allocations with tracemalloc for the period.
    #[arg(long, conflicts_with_all = ["snapshot", "diff", "drop"])]
    trace: bool,

    /// Keep a tracemalloc snapshot in each worker, starting tracemalloc
    /// if it is off, for a later --diff.
    #[arg(long, conflicts_with_all = ["diff", "drop"])]
    snapshot: bool,

    /// Show the sites that allocated most since the kept snapshot.
    #[arg(long, conflicts_with = "drop")]
    diff: bool,

    /// Drop the kept snapshot, stopping tracemalloc if --snapshot started it.
    #[arg(long)]
    drop: bool,

    /// How long to trace allocations.
    #[arg(long = "for", value_name = "DURATION", default_value = "30s", value_parser = muru_config::parse_duration)]
    duration: Duration,

    /// How many object types and allocation sites to show.
    #[arg(long, value_name = "N", default_value_t = 20)]
    top: u32,
}

/// `muructl debug imports`.
#[derive(Args)]
pub struct ImportsArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// Only the imports made while the host loaded one of its targets:
    /// hooks, entrypoint, mount:NAME or module:NAME (one of the
    /// preload_modules), or none for those made outside them, by an
    /// instrumentation module or the on_load and pre_fork hooks.
    #[arg(long = "target", id = "load_target", value_name = "TARGET")]
    load_target: Option<String>,

    /// The most rows of each table.
    #[arg(long, default_value_t = crate::imports::DEFAULT_TOP, value_name = "N")]
    top: usize,

    /// Only rows that took at least this many milliseconds.
    #[arg(long, default_value_t = 0.0, value_name = "MS")]
    min_ms: f64,

    /// Only modules and importers under this top-level package;
    /// repeatable.
    #[arg(long, value_name = "PACKAGE")]
    package: Vec<String>,

    /// The chain of imports that brought one module in, outermost first,
    /// in place of the tables.
    #[arg(long, value_name = "NAME")]
    module: Option<String>,

    /// Every row of the imports table, with those nothing can be done
    /// about: imports inside a listed import's load, and those no way of
    /// deferring can defer, such as an extension module's.
    #[arg(long)]
    all: bool,
}

impl ImportsArgs {
    /// The rows the command keeps.
    pub fn filters(&self) -> crate::imports::Filters {
        crate::imports::Filters {
            target: self.load_target.clone(),
            top: self.top,
            min_ms: self.min_ms,
            packages: self.package.clone(),
            module: self.module.clone(),
            all: self.all,
        }
    }
}

/// `muructl debug exec`.
#[derive(Args)]
pub struct ExecArgs {
    #[command(flatten)]
    common: DebugArgs,

    /// How long the code may run.
    #[arg(long, value_name = "DURATION", default_value = "30s", value_parser = muru_config::parse_duration)]
    timeout: Duration,

    /// The code, after --.
    #[arg(last = true, required = true, value_name = "CODE")]
    code: Vec<String>,
}

/// The logging subcommands (section 11.4).
#[derive(Subcommand)]
pub enum LoggingCommand {
    /// Set loggers' levels in a release's workers, for a period or until
    /// reset; with no level and no --reset, list the levels changed this
    /// way in each worker and until when.
    Level {
        /// The release as app@release, or an application for its current
        /// release; the one application of `muructl run` or `muructl
        /// serve` when left out.
        #[arg(value_name = "APP[@RELEASE]")]
        target: Option<String>,

        /// A logger and its level as NAME=LEVEL (root with an empty name,
        /// as =DEBUG). Repeatable.
        #[arg(value_name = "NAME=LEVEL")]
        levels: Vec<String>,

        /// Revert on its own after this long, for example 10m.
        #[arg(long = "for", value_name = "DURATION", value_parser = muru_config::parse_duration)]
        duration: Option<Duration>,

        /// Put every level changed this way back.
        #[arg(long)]
        reset: bool,

        /// One worker instead of every worker of the release: its index
        /// within its group, as GROUP/N outside the default group.
        #[arg(long, value_name = "[GROUP/]N")]
        worker: Option<WorkerName>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
    /// Change which requests the access log writes, for a period, or list
    /// the changes in force.
    Access {
        /// on, errors or off; left out, the changes in force are listed.
        #[arg(value_name = "MODE", requires = "duration")]
        mode: Option<String>,

        /// One application's requests, instead of every application's.
        #[arg(long, value_name = "APP")]
        app: Option<String>,

        /// Only requests whose path matches (`*` matches anything, `?` one
        /// character), written even where the configuration leaves them
        /// out. Repeatable.
        #[arg(long = "path", value_name = "PATTERN")]
        paths: Vec<String>,

        /// How long the change lasts, for example 10m; it then reverts on
        /// its own.
        #[arg(long = "for", value_name = "DURATION", value_parser = muru_config::parse_duration)]
        duration: Option<Duration>,

        /// Put the changes back: every one, or those for --app.
        #[arg(long, conflicts_with_all = ["mode", "paths", "duration"])]
        reset: bool,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// Ask a release's workers something and print what came back.
pub async fn debug(command: DebugCommand) -> anyhow::Result<()> {
    if let DebugCommand::Attach(args) = &command {
        return attach(args).await;
    }

    // The diagnostic commands look at the whole installation, not into
    // one release's workers.

    let command = match command {
        DebugCommand::Report(args) => return crate::diagnostics::report::report(args).await,
        DebugCommand::Bundle(args) => return crate::diagnostics::bundle::bundle(args).await,
        DebugCommand::Request(args) => return request_record(args).await,
        other => other,
    };

    if let DebugCommand::Profile(profile) = &command
        && profile.format == ProfileFormat::Pstats
        && profile.output.is_none()
    {
        anyhow::bail!("--format pstats writes a binary file, so it needs --output");
    }

    // Section 11.7: an operation over a period gives the host that long
    // and a little more before it gives up on a worker.

    let period = |duration: Duration| duration.as_secs_f64() + 5.0;

    let (op, args, request) = match &command {
        DebugCommand::Status(args) => ("status", args, serde_json::json!({})),
        DebugCommand::Stacks(args) => ("stacks", args, serde_json::json!({})),
        DebugCommand::Tasks(args) => ("tasks", args, serde_json::json!({})),
        DebugCommand::Inspect(inspect) => (
            "inspect",
            &inspect.common,
            serde_json::json!({"env": inspect.env}),
        ),
        DebugCommand::Logging(setup) => ("logging_setup", &setup.common, serde_json::json!({})),
        DebugCommand::Profile(profile) => (
            "profile",
            &profile.common,
            serde_json::json!({
                "duration_s": profile.duration.as_secs_f64(),
                "interval_ms": profile.interval.as_secs_f64() * 1000.0,
                "requests": profile.requests.unwrap_or(0),
                "path": profile.path.clone().unwrap_or_default(),
                "timeout_s": period(profile.duration),
            }),
        ),
        DebugCommand::Trace(trace) => (
            "trace",
            &trace.common,
            serde_json::json!({
                "duration_s": trace.duration.as_secs_f64(),
                "force": trace.force,
                "top": trace.top,
                "timeout_s": period(trace.duration),
            }),
        ),
        DebugCommand::Memory(memory) => (
            "memory",
            &memory.common,
            serde_json::json!({
                "trace": memory.trace,
                "snapshot": if memory.snapshot {
                    "take"
                } else if memory.diff {
                    "diff"
                } else if memory.drop {
                    "drop"
                } else {
                    ""
                },
                "duration_s": memory.duration.as_secs_f64(),
                "top": memory.top,
                "timeout_s": if memory.trace { period(memory.duration) } else { 30.0 },
            }),
        ),
        DebugCommand::Exec(exec) => (
            "exec",
            &exec.common,
            serde_json::json!({
                "code": exec.code.join(" "),
                "timeout_s": exec.timeout.as_secs_f64(),
            }),
        ),
        DebugCommand::Imports(imports) => ("imports", &imports.common, serde_json::json!({})),
        DebugCommand::Attach(_)
        | DebugCommand::Report(_)
        | DebugCommand::Bundle(_)
        | DebugCommand::Request(_) => {
            unreachable!("attach, report and bundle are handled above")
        }
    };

    let target = args.target.as_deref().unwrap_or_default();
    let response = ask(&args.socket, target, op, args.worker.as_ref(), request).await?;

    // Section 6.18: the import report is one document of the host's
    // answer and the workers', in text or as JSON.

    if let DebugCommand::Imports(imports) = &command {
        let worker = args.worker.as_ref().map(ToString::to_string);
        let found = crate::imports::document(&response, worker.as_deref(), &imports.filters())?;

        if args.json {
            println!("{}", serde_json::to_string_pretty(&found)?);
        } else {
            print!("{}", crate::imports::render(&found));
        }

        return Ok(());
    }

    if args.json {
        print_json(&response);
        return Ok(());
    }

    match &command {
        DebugCommand::Status(_) => print_status(&response),
        DebugCommand::Stacks(_) => print_stacks(&response),
        DebugCommand::Tasks(_) => print_tasks(&response),
        DebugCommand::Inspect(_) => print_inspect(&response),
        DebugCommand::Logging(setup) => print_logging_setup(&response, setup.all),
        DebugCommand::Profile(profile) => write_profile(&response, profile)?,
        DebugCommand::Trace(_) => print_trace(&response),
        DebugCommand::Memory(_) => print_memory(&response),
        DebugCommand::Exec(_) => print_exec(&response),
        DebugCommand::Attach(_)
        | DebugCommand::Report(_)
        | DebugCommand::Bundle(_)
        | DebugCommand::Request(_)
        | DebugCommand::Imports(_) => {}
    }

    Ok(())
}

/// How often a session with nothing typed asks for pdb's output.
const ATTACH_POLL: Duration = Duration::from_millis(300);

/// `muructl debug attach` (section 11.7): a pdb session carried line by
/// line, each line read going to the worker with a request whose answer
/// brings what pdb wrote since, and the session polled while nothing is
/// typed.
async fn attach(attach_args: &AttachArgs) -> anyhow::Result<()> {
    use std::io::Write as _;

    let args = &attach_args.common;

    let worker = match &args.worker {
        Some(worker) => worker.clone(),
        None => {
            let response = ask(
                &args.socket,
                args.target.as_deref().unwrap_or_default(),
                "breakpoints",
                None,
                serde_json::json!({}),
            )
            .await?;
            let waiting = response.workers.iter().find(|answer| {
                parse_result(answer).is_some_and(|result| {
                    result["waiting"]
                        .as_array()
                        .is_some_and(|waiting| !waiting.is_empty())
                })
            });

            waiting
                .or(response.workers.first())
                .map_or_else(|| WorkerName::new("", 0), answered_name)
        }
    };

    let worker = &worker;
    let exchange = |op: &'static str, request: serde_json::Value| async move {
        let target = args.target.as_deref().unwrap_or_default();
        let response = ask(&args.socket, target, op, Some(worker), request).await?;
        let answer = response
            .workers
            .first()
            .ok_or_else(|| anyhow::anyhow!("worker {worker} did not answer"))?;

        parse_result(answer).ok_or_else(|| anyhow::anyhow!("worker {worker}: {}", answer.error))
    };

    if attach_args.dap {
        return relay_dap(attach_args.listen, worker, exchange).await;
    }

    let opened = exchange("attach", serde_json::json!({})).await?;
    let session = opened["session"].as_u64().unwrap_or_default();
    eprintln!(
        "muru: attached to worker {worker} at {}; the end of input lets the code go on",
        opened["where"].as_str().unwrap_or("?")
    );
    print!("{}", opened["output"].as_str().unwrap_or_default());
    std::io::stdout().flush()?;

    // Lines are read on a thread of their own, since reading blocks.

    let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();

    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else {
                break;
            };

            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });

    loop {
        let (op, request) = match tokio::time::timeout(ATTACH_POLL, lines.recv()).await {
            Ok(Some(line)) => (
                "attach_io",
                serde_json::json!({"session": session, "input": line, "wait_s": 0.5}),
            ),
            Ok(None) => ("detach", serde_json::json!({"session": session})),
            Err(_) => (
                "attach_io",
                serde_json::json!({"session": session, "input": "", "wait_s": 0.0}),
            ),
        };

        let result = exchange(op, request).await?;
        print!("{}", result["output"].as_str().unwrap_or_default());
        std::io::stdout().flush()?;

        if op == "detach" || result["ended"].as_bool().unwrap_or(true) {
            eprintln!("muru: the session in worker {worker} has ended");
            return Ok(());
        }
    }
}

/// `muructl debug attach --dap` (section 11.7): the Debug Adapter
/// Protocol between an IDE connected to `listen` and debugpy's adapter in
/// the worker, what the IDE sent going with each request and what the
/// adapter sent coming back with its answer, base64, and the session
/// polled while the IDE is quiet. The IDE closing its connection ends it.
async fn relay_dap<F, R>(listen: SocketAddr, worker: &WorkerName, exchange: F) -> anyhow::Result<()>
where
    F: Fn(&'static str, serde_json::Value) -> R,
    R: std::future::Future<Output = anyhow::Result<serde_json::Value>>,
{
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    eprintln!(
        "muru: waiting for an IDE to attach to {} with the Debug Adapter Protocol",
        listener.local_addr()?
    );
    let (ide, _) = listener.accept().await?;
    drop(listener);

    let opened = exchange("attach", serde_json::json!({"dap": true})).await?;
    let session = opened["session"].as_u64().unwrap_or_default();
    eprintln!(
        "muru: attached to worker {worker} at {}; the IDE disconnecting lets the code go on",
        opened["where"].as_str().unwrap_or("?")
    );

    let (mut from_ide, mut to_ide) = ide.into_split();
    let mut buffer = vec![0; 65536];

    loop {
        let (op, request) =
            match tokio::time::timeout(ATTACH_POLL, from_ide.read(&mut buffer)).await {
                Ok(Ok(0) | Err(_)) => ("detach", serde_json::json!({"session": session})),
                Ok(Ok(read)) => (
                    "attach_io",
                    serde_json::json!({
                        "session": session,
                        "data": STANDARD.encode(&buffer[..read]),
                        "wait_s": 0.5,
                    }),
                ),
                Err(_) => (
                    "attach_io",
                    serde_json::json!({"session": session, "data": "", "wait_s": 0.0}),
                ),
            };

        let result = exchange(op, request).await?;
        let sent = STANDARD
            .decode(result["data"].as_str().unwrap_or_default())
            .context("the worker's answer")?;

        // An IDE that has gone ends the session at the next turn.

        if !sent.is_empty() && to_ide.write_all(&sent).await.is_err() {
            exchange("detach", serde_json::json!({"session": session})).await?;
            eprintln!("muru: the session in worker {worker} has ended");
            return Ok(());
        }

        if op == "detach" || result["ended"].as_bool().unwrap_or(true) {
            eprintln!("muru: the session in worker {worker} has ended");
            return Ok(());
        }
    }
}

/// Merge the workers' collapsed stacks and write them, heaviest first, or
/// as pstats, with the functions that took most samples on standard
/// error.
fn write_profile(response: &pb::DebugResponse, profile: &ProfileArgs) -> anyhow::Result<()> {
    use std::io::Write as _;

    let output = profile.output.as_deref();
    let mut stacks: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut leaves: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut rounds = 0;
    let mut requests = 0;

    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            eprintln!(
                "worker {} (pid {}): {}",
                answered_name(answer),
                answer.pid,
                answer.error
            );
            continue;
        };

        rounds += result["rounds"].as_u64().unwrap_or(0);
        requests += result["requests"].as_u64().unwrap_or(0);

        for (stack, count) in result["stacks"].as_object().into_iter().flatten() {
            let count = count.as_u64().unwrap_or(0);
            *stacks.entry(stack.clone()).or_default() += count;

            if let Some(leaf) = stack.rsplit(';').next() {
                *leaves.entry(leaf.to_owned()).or_default() += count;
            }
        }
    }

    let mut ordered: Vec<(&String, &u64)> = stacks.iter().collect();
    ordered.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let mut text = String::new();

    for (stack, count) in ordered {
        text.push_str(&format!("{stack} {count}\n"));
    }

    let bytes = match profile.format {
        ProfileFormat::Collapsed => text.into_bytes(),
        ProfileFormat::Pstats => crate::pstats::encode(
            stacks.iter().map(|(stack, count)| (stack.as_str(), *count)),
            profile.interval.as_secs_f64(),
        ),
    };

    match output {
        Some(path) => {
            std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?
        }
        None => std::io::stdout()
            .write_all(&bytes)
            .inspect_err(muru_common::log::reader_gone)?,
    }

    let total: u64 = leaves.values().sum();
    let mut busiest: Vec<(String, u64)> = leaves.into_iter().collect();
    busiest.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    eprintln!(
        "{total} samples of running Python code in {rounds} rounds across {} worker{}",
        response.workers.len(),
        if response.workers.len() == 1 { "" } else { "s" }
    );

    for (function, count) in busiest.iter().take(10) {
        eprintln!(
            "  {:>5.1}%  {function}",
            *count as f64 * 100.0 / total.max(1) as f64
        );
    }

    if profile.requests.is_some() || profile.path.is_some() {
        eprintln!(
            "{requests} request{} sampled to the end",
            if requests == 1 { "" } else { "s" }
        );
    }

    if let Some(path) = output {
        let what = match profile.format {
            ProfileFormat::Collapsed => "collapsed stacks",
            ProfileFormat::Pstats => "pstats",
        };

        eprintln!("muru: {what} written to {}", path.display());
    }

    Ok(())
}

fn print_trace(response: &pb::DebugResponse) {
    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}\n",
                answered_name(answer),
                answer.pid,
                answer.error
            );
            continue;
        };

        println!("worker {} (pid {})", answered_name(answer), answer.pid);
        println!("  {:>10}  {:>8}  function", "total ms", "calls");

        for function in result["functions"].as_array().into_iter().flatten() {
            println!(
                "  {:>10.3}  {:>8}  {}",
                function["total_ms"].as_f64().unwrap_or(0.0),
                function["calls"].as_u64().unwrap_or(0),
                function["function"].as_str().unwrap_or("?")
            );
        }

        println!();
    }
}

fn print_memory(response: &pb::DebugResponse) {
    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}\n",
                answered_name(answer),
                answer.pid,
                answer.error
            );
            continue;
        };

        println!("worker {} (pid {})", answered_name(answer), answer.pid);

        if let Some(memory) = result["memory"].as_object() {
            let size = |name: &str| {
                memory
                    .get(name)
                    .and_then(serde_json::Value::as_u64)
                    .map(muru_config::format_size_approx)
                    .unwrap_or_else(|| "?".to_owned())
            };
            println!(
                "  rss {}  pss {}  uss {}",
                size("rss"),
                size("pss"),
                size("uss")
            );
        }

        println!("  gc counts {}", result["gc"]["counts"]);

        // Section 11.1: the collections the runtime timed, by generation.

        for generation in result["gc"]["pauses"].as_array().into_iter().flatten() {
            let count = |key: &str| generation[key].as_u64().unwrap_or(0);
            let pauses: Vec<u64> = ["idle_hist", "busy_hist"]
                .iter()
                .map(|key| {
                    generation[*key]
                        .as_array()
                        .map(|items| items.iter().filter_map(serde_json::Value::as_u64).collect())
                        .unwrap_or_default()
                })
                .reduce(|idle: Vec<u64>, busy: Vec<u64>| {
                    idle.iter().zip(&busy).map(|(a, b)| a + b).collect()
                })
                .unwrap_or_default();
            let p95 = signal_p95_ms(&pauses).map_or("-".to_owned(), |ms| {
                muru_controller::scaling::describe_p95(
                    ms,
                    muru_controller::scaling::SIGNAL_LARGEST_MS,
                )
            });
            println!(
                "  gc generation {}: {} collections, {:.1} ms in all, p95 {}, {} objects freed, {} while {} requests were in flight",
                generation["generation"].as_str().unwrap_or("?"),
                count("collections"),
                count("ns") as f64 / 1e6,
                p95,
                count("collected"),
                count("busy"),
                count("waited"),
            );
        }

        println!("  objects:");

        for entry in result["objects"].as_array().into_iter().flatten() {
            println!(
                "    {:>9}  {}",
                entry[1].as_u64().unwrap_or(0),
                entry[0].as_str().unwrap_or("?")
            );
        }

        match result["snapshot"].as_str() {
            Some("taken") => println!(
                "  snapshot kept; tracemalloc holds {} (peak {})",
                muru_config::format_size_approx(result["traced"].as_u64().unwrap_or(0)),
                muru_config::format_size_approx(result["peak"].as_u64().unwrap_or(0))
            ),
            Some("dropped") => println!("  snapshot dropped"),
            Some("none") => println!("  no snapshot was kept"),
            _ => {}
        }

        if let Some(allocations) = result["allocations"].as_array() {
            if result["snapshot"] == "compared" {
                println!("  allocated since the snapshot:");
            } else {
                println!("  allocated over the period:");
            }

            for site in allocations {
                println!(
                    "    {:>+12}  {:>+8}  {}",
                    site["size_diff"].as_i64().unwrap_or(0),
                    site["count_diff"].as_i64().unwrap_or(0),
                    site["site"].as_str().unwrap_or("?")
                );
            }
        }

        println!();
    }
}

fn print_exec(response: &pb::DebugResponse) {
    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}",
                answered_name(answer),
                answer.pid,
                answer.error
            );
            continue;
        };

        println!("worker {} (pid {}):", answered_name(answer), answer.pid);
        print!("{}", result["output"].as_str().unwrap_or_default());

        if let Some(value) = result["value"].as_str() {
            println!("{value}");
        }
    }
}

/// Change logger levels in a release's workers, or which requests the
/// access log writes.
pub async fn logging(command: LoggingCommand) -> anyhow::Result<()> {
    let (target, mut levels, duration, reset, worker, socket, output) = match command {
        LoggingCommand::Level {
            target,
            levels,
            duration,
            reset,
            worker,
            socket,
            output,
        } => (target, levels, duration, reset, worker, socket, output),
        LoggingCommand::Access {
            mode,
            app,
            paths,
            duration,
            reset,
            socket,
            output,
        } => {
            return access_log(AccessChange {
                mode,
                app,
                paths,
                duration,
                reset,
                socket,
                json: output.json,
            })
            .await;
        }
    };

    // With the target left out, the first NAME=LEVEL arrives in its place.

    let target = match target {
        Some(setting) if setting.contains('=') => {
            levels.insert(0, setting);
            String::new()
        }
        target => target.unwrap_or_default(),
    };

    // No level and no reset: what each worker has changed, and until when.

    if !reset && levels.is_empty() {
        anyhow::ensure!(
            duration.is_none(),
            "--for needs a logger and its level as NAME=LEVEL"
        );

        let response = ask(
            &socket,
            &target,
            "logging",
            worker.as_ref(),
            serde_json::json!({"list": true}),
        )
        .await?;

        if output.json {
            return print_answer(&[debug_json(&response)]);
        }

        for answer in &response.workers {
            let Some(result) = parse_result(answer) else {
                println!(
                    "worker {} (pid {}): {}",
                    answered_name(answer),
                    answer.pid,
                    answer.error
                );
                continue;
            };

            let changed = result["changed"].as_array().cloned().unwrap_or_default();

            if changed.is_empty() {
                println!(
                    "worker {} (pid {}): no levels changed",
                    answered_name(answer),
                    answer.pid
                );
            }

            for entry in &changed {
                let until = entry["until_unix_ms"].as_u64().map_or_else(
                    || "until reset".to_owned(),
                    |ms| format!("until {}", format_time(ms)),
                );
                println!(
                    "worker {} (pid {}): {} {} (was {}), {until}",
                    answered_name(answer),
                    answer.pid,
                    entry["logger"].as_str().unwrap_or("root"),
                    entry["level"].as_str().unwrap_or("?"),
                    entry["previous"].as_str().unwrap_or("?"),
                );
            }
        }

        return Ok(());
    }

    if reset {
        let response = ask(
            &socket,
            &target,
            "logging",
            worker.as_ref(),
            serde_json::json!({"reset": true}),
        )
        .await?;

        if output.json {
            return print_answer(&[debug_json(&response)]);
        }

        for answer in &response.workers {
            match parse_result(answer) {
                Some(result) => println!(
                    "worker {} (pid {}): reset {}",
                    answered_name(answer),
                    answer.pid,
                    result["reset"]
                        .as_array()
                        .map(|names| {
                            names
                                .iter()
                                .filter_map(|name| name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .filter(|text| !text.is_empty())
                        .unwrap_or_else(|| "nothing".to_owned())
                ),
                None => println!(
                    "worker {} (pid {}): {}",
                    answered_name(answer),
                    answer.pid,
                    answer.error
                ),
            }
        }

        return Ok(());
    }

    // With --json, one debug answer for each logger set, as one array.

    let mut answers = Vec::new();

    for setting in levels {
        let (name, level) = setting.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("{setting:?} is not NAME=LEVEL, for example myapp=DEBUG")
        })?;
        let args = serde_json::json!({
            "logger": name,
            "level": level,
            "duration_s": duration.map(|duration| duration.as_secs_f64()),
        });
        let response = ask(&socket, &target, "logging", worker.as_ref(), args).await?;

        if output.json {
            answers.push(debug_json(&response));
            continue;
        }

        for answer in &response.workers {
            match parse_result(answer) {
                Some(result) => println!(
                    "worker {} (pid {}): {} {} (was {}){}",
                    answered_name(answer),
                    answer.pid,
                    result["logger"].as_str().unwrap_or("root"),
                    result["level"].as_str().unwrap_or(level),
                    result["previous"].as_str().unwrap_or("?"),
                    duration
                        .map(|duration| format!(", for {}", muru_config::format_duration(duration)))
                        .unwrap_or_default()
                ),
                None => println!(
                    "worker {} (pid {}): {}",
                    answered_name(answer),
                    answer.pid,
                    answer.error
                ),
            }
        }
    }

    if output.json {
        return print_answer(&answers);
    }

    Ok(())
}

/// What `muructl logging access` asks for.
struct AccessChange {
    mode: Option<String>,
    app: Option<String>,
    paths: Vec<String>,
    duration: Option<Duration>,
    reset: bool,
    socket: SocketArgs,
    json: bool,
}

/// Change which requests the access log writes for a period, put the
/// changes back, or list those in force (section 11.2).
async fn access_log(change: AccessChange) -> anyhow::Result<()> {
    let mut client = client::connect(&change.socket.endpoint()?).await?;
    let answer = client
        .override_access_log(pb::OverrideAccessLogRequest {
            mode: change.mode.clone().unwrap_or_default(),
            app: change.app.clone().unwrap_or_default(),
            paths: change.paths.clone(),
            duration_ms: change.duration.map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            }),
            reset: change.reset,
        })
        .await
        .map_err(explain)?
        .into_inner();

    if change.json {
        return print_answer(&answer);
    }

    if change.reset {
        println!(
            "muru: put back {} change{} to the access log",
            answer.reset,
            if answer.reset == 1 { "" } else { "s" }
        );
    } else if let (Some(added), Some(duration)) = (answer.overrides.last(), change.duration)
        && change.mode.is_some()
    {
        println!(
            "muru: the access log writes {} for {}, then goes back on its own",
            muru_edge::access::Override::from_admin_message(added).describe(),
            muru_edge::access::format_lifetime(duration)
        );
    }

    if answer.overrides.is_empty() {
        println!("muru: the access log writes what the configuration says");
        return Ok(());
    }

    // The newest decides where two cover a request, so it comes first.

    println!("muru: changes in force, the newest first:");
    let now = now_unix_ms();

    for listed in answer.overrides.iter().rev() {
        let left = Duration::from_millis(listed.until_unix_ms.saturating_sub(now));
        println!(
            "  {}, {} more",
            muru_edge::access::Override::from_admin_message(listed).describe(),
            muru_edge::access::format_lifetime(left)
        );
    }

    Ok(())
}

/// Send one debug request for a target: an application, or app@release.
async fn ask(
    socket: &SocketArgs,
    target: &str,
    op: &str,
    worker: Option<&WorkerName>,
    args: serde_json::Value,
) -> anyhow::Result<pb::DebugResponse> {
    let (app, release) = match target.split_once('@') {
        Some((app, release)) if !app.is_empty() && !release.is_empty() => (app, release),
        Some(_) => anyhow::bail!("{target:?} is not a release: expected app@release"),
        None => (target, ""),
    };

    let mut client = client::connect(&socket.endpoint()?).await?;
    let response = client
        .debug(pb::DebugRequest {
            app: app.to_owned(),
            release: release.to_owned(),
            op: op.to_owned(),
            worker: worker.map(|worker| worker.index),
            group: worker
                .map(|worker| worker.group.clone())
                .unwrap_or_default(),
            args_json: args.to_string(),
        })
        .await
        .map_err(|status| explain_debug(op, status))?
        .into_inner();

    if response.workers.is_empty() {
        anyhow::bail!(
            "{}@{} has no worker to ask{}",
            app,
            response.release,
            worker
                .map(|index| format!(" with index {index}"))
                .unwrap_or_default()
        );
    }

    Ok(response)
}

/// A worker's answer as JSON, when it answered.
/// The 95th percentile of a worker's cumulative histogram over the
/// signal bounds, in milliseconds: the bound holding 95 percent of the
/// observations, infinity past the last, or none without any.
fn signal_p95_ms(counts: &[u64]) -> Option<f64> {
    let bounds = muru_proto::worker::v1::SIGNAL_BOUNDS_US;
    let total = *counts.last()?;

    if total == 0 || counts.len() != bounds.len() + 1 {
        return None;
    }

    let target = total as f64 * 0.95;
    let index = counts.iter().position(|count| *count as f64 >= target)?;
    Some(
        bounds
            .get(index)
            .map_or(f64::INFINITY, |bound| *bound as f64 / 1000.0),
    )
}

fn parse_result(answer: &pb::WorkerDebug) -> Option<serde_json::Value> {
    if !answer.ok {
        return None;
    }

    serde_json::from_str(&answer.result_json).ok()
}

fn print_json(response: &pb::DebugResponse) {
    println!(
        "{}",
        serde_json::to_string_pretty(&debug_json(response)).unwrap_or_default()
    );
}

/// A worker's name, from its answer: its index within its group, after
/// the group's name and a slash outside the default group.
pub(crate) fn answered_name(answer: &pb::WorkerDebug) -> WorkerName {
    WorkerName::new(&answer.group, answer.index)
}

/// A debug answer as JSON: the release, and each worker's answer with its
/// result parsed.
pub(crate) fn debug_json(response: &pb::DebugResponse) -> serde_json::Value {
    let workers: Vec<serde_json::Value> = response
        .workers
        .iter()
        .map(|answer| {
            serde_json::json!({
                "index": answer.index,
                "group": answered_name(answer).group,
                "pid": answer.pid,
                "ok": answer.ok,
                "error": (!answer.error.is_empty()).then_some(answer.error.as_str()),
                "result": parse_result(answer),
            })
        })
        .collect();
    serde_json::json!({
        "release": response.release,
        "workers": workers,
    })
}

/// The status table (section 11.7): a row per worker, with the columns
/// of each side any worker of the release has, every header one word.
fn print_status(response: &pb::DebugResponse) {
    let rows = muru_dashboard::inspect::status_rows(&debug_answer(response));
    let sides = muru_dashboard::inspect::status_shape(&rows);
    fn cells(side: &Option<muru_dashboard::inspect::SideView>) -> (&str, &str) {
        match side {
            Some(side) => (side.busy.as_str(), side.oldest.as_str()),
            None => ("-", "-"),
        }
    }
    let mut header = format!(
        "{:<7} {:<8} {:<10} {:<8} {:<10} {:<8}",
        "WORKER", "PID", "GROUP", "UP", "REQUESTS", "MODULES"
    );

    if sides.threads {
        header += &format!(" {:<8} {:<14}", "THREADS", "THREAD-OLDEST");
    }

    if sides.event_loop {
        header += &format!(" {:<9} {:<12}", "LOOP", "LOOP-OLDEST");
    }

    if sides.sync {
        header += &format!(" {:<7} {:<12}", "SYNC", "SYNC-OLDEST");
    }

    println!("{header} {:<7} {:<7} {:<7} STATE", "RSS", "USS", "ROUTED");

    for row in &rows {
        if !row.error.is_empty() {
            println!("{:<7} {:<8} {}", row.worker, row.pid, row.error);
            continue;
        }

        let mut line = format!(
            "{:<7} {:<8} {:<10} {:<8} {:<10} {:<8}",
            row.worker, row.pid, row.group, row.uptime, row.requests, row.modules
        );

        if sides.threads {
            let (busy, oldest) = cells(&row.threads);
            line += &format!(" {busy:<8} {oldest:<14}");
        }

        if sides.event_loop {
            let (busy, oldest) = cells(&row.event_loop);
            line += &format!(" {busy:<9} {oldest:<12}");
        }

        if sides.sync {
            let (busy, oldest) = cells(&row.sync);
            line += &format!(" {busy:<7} {oldest:<12}");
        }

        println!(
            "{line} {:<7} {:<7} {:<7} {}",
            row.rss,
            row.uss,
            if row.routed { "yes" } else { "no" },
            row.state
        );
    }

    if sides.stale {
        println!(
            "* from the worker's last report, up to a second old: it did not answer within a second"
        );
    }
}

fn print_stacks(response: &pb::DebugResponse) {
    // Threads with the same frames across workers are shown once with
    // the workers they came from, merged as the dashboard merges them.

    for answer in &response.workers {
        if !answer.ok {
            println!(
                "worker {} (pid {}): {}\n",
                answered_name(answer),
                answer.pid,
                answer.error
            );
        }
    }

    for stack in muru_dashboard::inspect::merge_stacks(&debug_answer(response)) {
        println!("{} in {}:", stack.name, stack.where_);

        if let Some(serving) = &stack.request {
            println!(
                "  serving {} {} for {} (request {})",
                serving.method, serving.path, serving.elapsed, serving.request_id
            );
        }

        if stack.frames.is_empty() {
            println!("  (no Python frames)");
        }

        for frame in &stack.frames {
            println!(
                "  File \"{}\", line {}, in {}\n    {}",
                frame.file, frame.line, frame.function, frame.text
            );
        }

        println!();
    }
}

/// The workers' answers as the dashboard's views read them.
fn debug_answer(response: &pb::DebugResponse) -> muru_dashboard::DebugAnswer {
    muru_dashboard::DebugAnswer {
        release: response.release.clone(),
        workers: response
            .workers
            .iter()
            .map(|answer| muru_dashboard::WorkerAnswer {
                index: answer.index,
                group: answered_name(answer).group,
                pid: answer.pid,
                ok: answer.ok,
                error: (!answer.error.is_empty()).then(|| answer.error.clone()),
                result: parse_result(answer),
            })
            .collect(),
    }
}

fn print_tasks(response: &pb::DebugResponse) {
    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}",
                answered_name(answer),
                answer.pid,
                answer.error
            );
            continue;
        };

        match result["lag_ms"].as_f64() {
            Some(lag) => println!(
                "worker {} (pid {}): loop lag {lag:.1} ms",
                answered_name(answer),
                answer.pid
            ),
            None => {
                println!(
                    "worker {} (pid {}): {}",
                    answered_name(answer),
                    answer.pid,
                    result["note"].as_str().unwrap_or("no event loop")
                );
                continue;
            }
        }

        let tasks = result["tasks"].as_array().cloned().unwrap_or_default();

        if tasks.is_empty() {
            println!("  no tasks");
        }

        for task in tasks {
            let at = task["at"]
                .as_object()
                .map(|at| {
                    format!(
                        " at {}:{}",
                        at["file"].as_str().unwrap_or("?"),
                        at["line"].as_u64().unwrap_or(0)
                    )
                })
                .unwrap_or_default();
            println!(
                "  {} {}{}{}",
                task["name"].as_str().unwrap_or("?"),
                task["coro"].as_str().unwrap_or("?"),
                task["waiting_on"]
                    .as_str()
                    .map(|waiting| format!(" waiting on {waiting}"))
                    .unwrap_or_default(),
                at
            );
        }

        println!();
    }
}

/// The logging setup as `debug logging` prints it. Workers that answer
/// the same are shown once, with the others named as matching; when they
/// differ, each is shown.
fn print_logging_setup(response: &pb::DebugResponse, all: bool) {
    let mut shown: Vec<(serde_json::Value, Vec<String>)> = Vec::new();

    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}",
                answered_name(answer),
                answer.pid,
                answer.error
            );
            continue;
        };

        let who = format!("worker {} (pid {})", answered_name(answer), answer.pid);

        match shown.iter_mut().find(|(setup, _)| *setup == result) {
            Some((_, workers)) => workers.push(who),
            None => shown.push((result, vec![who])),
        }
    }

    if shown.len() > 1 {
        println!("the workers differ: each setup is shown with the workers that have it");
    }

    for (setup, workers) in &shown {
        match workers.as_slice() {
            [one] => println!("{one}:"),
            [first, rest @ ..] => println!("{first}, the same in {}:", rest.join(", ")),
            [] => continue,
        }

        print_one_logging_setup(setup, all);
    }
}

/// Where a logger's level came from, as `debug logging` says it.
fn level_from(origin: &serde_json::Value) -> &'static str {
    match origin.as_str() {
        Some("muru") => " (Muru's defaults)",
        Some("temporary") => " (muructl logging level)",
        Some("changed since") => " (changed since Muru set it)",
        Some("application") => " (set by the application or a library)",
        Some("python") => " (Python's default)",
        _ => "",
    }
}

/// A handler as one line of `debug logging`.
fn handler_line(handler: &serde_json::Value) -> String {
    let mut line = format!(
        "{}, level {}",
        handler["class"].as_str().unwrap_or("?"),
        handler["level"].as_str().unwrap_or("?")
    );

    if let Some(destination) = handler["destination"].as_str() {
        line.push_str(&format!(", to {destination}"));
    }

    if let Some(format) = handler["format"].as_str() {
        line.push_str(&format!(", format {format:?}"));
    }

    let filters: Vec<&str> = handler["filters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();

    if !filters.is_empty() {
        line.push_str(&format!(", filters {}", filters.join(", ")));
    }

    line
}

fn print_one_logging_setup(setup: &serde_json::Value, all: bool) {
    println!(
        "  logging.disable {}, warnings captured {}",
        setup["disable"].as_str().unwrap_or("off"),
        if setup["capture_warnings"].as_bool() == Some(true) {
            "yes"
        } else {
            "no"
        }
    );

    if setup["last_resort"].is_object() {
        println!("  last resort: {}", handler_line(&setup["last_resort"]));
    }

    println!("  loggers:");
    let mut hidden = 0;

    for logger in setup["loggers"].as_array().into_iter().flatten() {
        let name = logger["name"].as_str().unwrap_or("?");
        let level = logger["level"].as_str().unwrap_or("NOTSET");
        let handlers = logger["handlers"].as_array().cloned().unwrap_or_default();
        let propagates = logger["propagate"].as_bool() != Some(false);
        let disabled = logger["disabled"].as_bool() == Some(true);
        let interesting =
            name == "root" || level != "NOTSET" || !handlers.is_empty() || !propagates || disabled;

        if !all && !interesting {
            hidden += 1;
            continue;
        }

        let mut line = format!(
            "    {name}: level {level}{}, effective {}",
            level_from(&logger["level_from"]),
            logger["effective"].as_str().unwrap_or("?")
        );

        if !propagates {
            line.push_str(", does not propagate");
        }

        if disabled {
            line.push_str(", disabled");
        }

        println!("{line}");

        for handler in &handlers {
            println!("      handler {}", handler_line(handler));
        }
    }

    if hidden > 0 {
        println!(
            "    and {} with nothing of their own (--all lists them)",
            counted(hidden, "logger", "loggers")
        );
    }

    let filters = setup["warnings_filters"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    println!("  warnings filters, the first matching wins:");

    if filters.is_empty() {
        println!("    none");
    }

    for filter in &filters {
        println!(
            "    {}:{}:{}:{}:{}",
            filter["action"].as_str().unwrap_or("?"),
            filter["message"].as_str().unwrap_or(""),
            filter["category"].as_str().unwrap_or(""),
            filter["module"].as_str().unwrap_or(""),
            filter["lineno"].as_u64().unwrap_or(0)
        );
    }
}

fn print_inspect(response: &pb::DebugResponse) {
    for answer in &response.workers {
        let Some(result) = parse_result(answer) else {
            println!(
                "worker {} (pid {}): {}",
                answered_name(answer),
                answer.pid,
                answer.error
            );
            continue;
        };
        let settings = &result["settings"];

        // The process model already says free-threaded for WSGI on a
        // build with the GIL off (issue 371); ASGI's is async, so the
        // build is named after it.

        let process_model = settings["process_model"].as_str().unwrap_or("?");
        println!(
            "worker {} (pid {}): python {} at {}, {} in {} (group {}, {}{})",
            answered_name(answer),
            answer.pid,
            result["python"].as_str().unwrap_or("?"),
            result["executable"].as_str().unwrap_or("?"),
            settings["app"].as_str().unwrap_or("?"),
            result["cwd"].as_str().unwrap_or("?"),
            settings["group"].as_str().unwrap_or("default"),
            process_model,
            if settings["gil_enabled"].as_bool() == Some(false) && process_model != "free-threaded"
            {
                ", free-threaded"
            } else {
                ""
            }
        );
        println!("  sys.path:");

        for entry in result["sys_path"].as_array().into_iter().flatten() {
            println!("    {}", entry.as_str().unwrap_or("?"));
        }

        let modules = result["modules"].as_object().cloned().unwrap_or_default();
        let versioned: Vec<String> = modules
            .iter()
            .filter_map(|(name, module)| {
                module["version"]
                    .as_str()
                    .map(|version| format!("{name} {version}"))
            })
            .collect();
        println!(
            "  {} modules loaded{}{}",
            modules.len(),
            if versioned.is_empty() {
                ""
            } else {
                ", with versions: "
            },
            versioned.join(", ")
        );
        let threads: Vec<String> = result["threads"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|thread| thread["name"].as_str().unwrap_or("?").to_owned())
            .collect();
        println!("  threads: {}", threads.join(", "));

        if let Some(count) = result["descriptors"]["count"].as_u64() {
            println!("  {count} open descriptors");
        }

        // A variable whose value was not asked for is named alone, and
        // the heading says how to see the rest.

        let environment = result["environment"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let withheld = environment.values().filter(|value| value.is_null()).count();

        if withheld == 0 {
            println!("  environment:");
        } else {
            println!("  environment ({withheld} values left out; --env shows them):");
        }

        for (key, value) in &environment {
            match value.as_str() {
                Some(value) => println!("    {key}={value}"),
                None => println!("    {key}"),
            }
        }

        println!();
    }
}

/// Print the node's metrics as Prometheus text.
pub async fn metrics(socket: SocketArgs, json: bool) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let text = client
        .get_metrics(pb::GetMetricsRequest {})
        .await?
        .into_inner()
        .text;

    if json {
        return print_answer(&serde_json::json!({ "samples": metric_samples(&text) }));
    }

    print!("{text}");
    Ok(())
}

/// The samples of a Prometheus text exposition as JSON objects: each with
/// its metric name, the type its family declares, its labels and its
/// value. A value JSON cannot hold (NaN, +Inf, -Inf) is kept as text.
pub fn metric_samples(text: &str) -> Vec<serde_json::Value> {
    let mut types = std::collections::HashMap::new();
    let mut samples = Vec::new();

    for line in text.lines().map(str::trim) {
        if let Some(declared) = line.strip_prefix("# TYPE ") {
            if let Some((family, kind)) = declared.split_once(' ') {
                types.insert(family.to_owned(), kind.trim().to_owned());
            }

            continue;
        }

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((name, labels, rest)) = split_sample(line) else {
            continue;
        };
        let value = rest.split_whitespace().next().unwrap_or_default();
        let value = value
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| serde_json::Value::from(value), serde_json::Value::Number);

        // A histogram's or summary's series belong to the family their
        // suffix names.

        let family = ["_bucket", "_sum", "_count", "_total"]
            .iter()
            .find_map(|suffix| {
                name.strip_suffix(suffix)
                    .filter(|base| types.contains_key(*base))
            })
            .unwrap_or(name);

        samples.push(serde_json::json!({
            "name": name,
            "type": types.get(family).map_or("untyped", String::as_str),
            "labels": labels,
            "value": value,
        }));
    }

    samples
}

/// A sample line's metric name, its labels, and what follows them (the
/// value and an optional timestamp).
fn split_sample(line: &str) -> Option<(&str, serde_json::Map<String, serde_json::Value>, &str)> {
    let Some(open) = line.find('{') else {
        let (name, rest) = line.split_once(char::is_whitespace)?;
        return Some((name, serde_json::Map::new(), rest));
    };
    let name = &line[..open];
    let mut labels = serde_json::Map::new();
    let mut chars = line[open + 1..].char_indices();

    // Label values are quoted, with \\, \" and \n escaped.

    loop {
        let (start, first) = chars
            .by_ref()
            .find(|(_, c)| !c.is_whitespace() && *c != ',')?;

        if first == '}' {
            let rest = &line[open + 1 + start + 1..];
            return Some((name, labels, rest));
        }

        let mut key = String::from(first);

        for (_, c) in chars.by_ref() {
            if c == '=' {
                break;
            }

            key.push(c);
        }

        chars.by_ref().find(|(_, c)| *c == '"')?;
        let mut value = String::new();

        while let Some((_, c)) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next()?.1 {
                    'n' => value.push('\n'),
                    other => value.push(other),
                },
                other => value.push(other),
            }
        }

        labels.insert(key.trim().to_owned(), serde_json::Value::from(value));
    }
}

/// What `muructl websockets` shows.
#[derive(Args)]
pub struct WebSocketsArgs {
    /// Only this application's WebSockets.
    pub app: Option<String>,

    /// List each open WebSocket, the longest open first, instead of
    /// counting them by endpoint.
    #[arg(long)]
    pub list: bool,

    /// How many to list with --list.
    #[arg(long, default_value = "100", value_name = "COUNT")]
    pub limit: u32,

    #[command(flatten)]
    pub socket: SocketArgs,

    #[command(flatten)]
    pub output: JsonArgs,
}

/// Show the WebSockets open now: counted by endpoint, where segments
/// that are identifiers are {id} and many distinct values *, or listed.
pub async fn websockets(args: WebSocketsArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let list = client
        .list_web_sockets(pb::ListWebSocketsRequest {
            app: args.app.unwrap_or_default(),
            limit: if args.list { args.limit } else { 0 },
        })
        .await
        .map_err(explain)?
        .into_inner();

    if args.output.json {
        return print_answer(&list);
    }

    if list.open == 0 {
        println!("no WebSockets open");
        return Ok(());
    }

    if !args.list {
        println!("{:<16} {:<40} {:>6}", "APP", "ENDPOINT", "OPEN");

        for endpoint in &list.endpoints {
            println!(
                "{:<16} {:<40} {:>6}",
                endpoint.app, endpoint.pattern, endpoint.open
            );
        }

        return Ok(());
    }

    println!(
        "{:<20} {:<9} {:<16} {:<32} {:<16} {:>8} {:>8} {:<6} REQUEST-ID",
        "OPENED", "FOR", "RELEASE", "PATH", "CLIENT", "RECEIVED", "SENT", "WORKER"
    );

    for socket in &list.sockets {
        println!(
            "{:<20} {:<9} {:<16} {:<32} {:<16} {:>8} {:>8} {:<6} {}",
            format_time(socket.opened_unix_ms),
            muru_dashboard::uptime(std::time::Duration::from_millis(
                now_unix_ms().saturating_sub(socket.opened_unix_ms)
            )),
            format!("{}@{}", socket.app, socket.release),
            socket.path,
            socket.client,
            socket.received,
            socket.sent,
            if socket.worker < 0 {
                "-".to_owned()
            } else {
                socket.worker.to_string()
            },
            socket.request_id
        );
    }

    if list.open as usize > list.sockets.len() {
        println!(
            "({} of {} open shown; --limit shows more)",
            list.sockets.len(),
            list.open
        );
    }

    Ok(())
}

/// Print a development run's record of one request (section 11.7).
async fn request_record(args: RequestRecordArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let record = client
        .get_request(pb::GetRequestRequest {
            request_id: args.request_id,
        })
        .await
        .map_err(explain)?
        .into_inner();

    if args.output.json {
        return print_answer(&record);
    }

    print!("{}", request_text(&record));
    Ok(())
}

/// A request's record as text: a line naming the request, then each
/// panel's spans, values and tables.
fn request_text(record: &pb::RequestRecord) -> String {
    use std::fmt::Write as _;

    let millis =
        |ms: f64| muru_edge::access::format_duration(Duration::from_secs_f64(ms.max(0.0) / 1000.0));
    let at = muru_common::log::clock(
        std::time::UNIX_EPOCH + Duration::from_millis(record.started_unix_ms),
    );
    let mut text = String::new();

    if record.in_progress {
        let _ = writeln!(
            text,
            "{} {} {}  in progress  at {at}",
            record.method, record.target, record.version
        );
    } else {
        let _ = writeln!(
            text,
            "{} {} {}  {}  {}  at {at}",
            record.method,
            record.target,
            record.version,
            record.status,
            millis(record.total_ms)
        );
    }

    let _ = writeln!(text, "request {}", record.request_id);

    for panel in &record.panels {
        let _ = writeln!(text, "\n{}", panel.title);

        // Spans indented by depth, each with where it starts on the
        // request's clock, and a failed one marked: an event at its
        // moment, and a span of several segments with their total and
        // count (issue 737).

        let mut spans: Vec<(usize, &pb::PanelSpan)> =
            panel.spans.iter().rev().map(|span| (0, span)).collect();
        let mut lines = Vec::new();

        while let Some((depth, span)) = spans.pop() {
            let failed = if span.failed { "  failed" } else { "" };

            let shown = match (span.at_ms, span.segments.as_slice()) {
                (Some(at), _) => format!("at {}{failed}", millis(at)),
                (None, [only]) => format!(
                    "{}  from {}{failed}",
                    millis(only.duration_ms),
                    millis(only.start_ms)
                ),
                (None, [first, ..]) => format!(
                    "{} in {} segments  from {}{failed}",
                    millis(
                        span.segments
                            .iter()
                            .map(|segment| segment.duration_ms)
                            .sum()
                    ),
                    span.segments.len(),
                    millis(first.start_ms)
                ),
                (None, []) => format!(
                    "{}  from {}{failed}",
                    millis(span.duration_ms),
                    millis(span.start_ms)
                ),
            };

            lines.push((format!("{}{}", "  ".repeat(depth), span.name), shown));
            spans.extend(span.children.iter().rev().map(|child| (depth + 1, child)));
        }

        lines.extend(
            panel
                .values
                .iter()
                .map(|value| (value.name.clone(), value.value.clone())),
        );
        let width = lines.iter().map(|(name, _)| name.len()).max().unwrap_or(0);

        for (name, value) in &lines {
            // A value of several lines, such as a traceback, goes below
            // its name, indented.

            if value.contains('\n') {
                let _ = writeln!(text, "  {name}");

                for line in value.lines() {
                    let _ = writeln!(text, "    {line}");
                }
            } else {
                let _ = writeln!(text, "  {name:<width$}  {value}");
            }
        }

        for table in &panel.tables {
            let _ = writeln!(text, "  {}", table.caption);

            if table.rows.is_empty() {
                let _ = writeln!(text, "    none");
            }

            for row in &table.rows {
                match row.cells.as_slice() {
                    // Issue 780: a value of several lines, such as a
                    // stack, goes below its name, every line indented, as
                    // a panel's own values do.
                    [name, value] if value.contains('\n') => {
                        let _ = writeln!(text, "    {name}");

                        for line in value.lines() {
                            let _ = writeln!(text, "      {line}");
                        }
                    }
                    [name, value] => {
                        let _ = writeln!(text, "    {name}: {value}");
                    }
                    cells => {
                        let line = cells.join("  ");
                        let mut lines = line.lines();
                        let _ = writeln!(text, "    {}", lines.next().unwrap_or_default());

                        for more in lines {
                            let _ = writeln!(text, "      {more}");
                        }
                    }
                }
            }
        }

        // Issue 680: a text panel's text, as written, markdown included.

        for line in panel.text.lines() {
            let _ = writeln!(text, "  {line}");
        }

        // Issue 677: what a panel from the worker could not keep.

        if panel.dropped > 0 {
            let entries = if panel.dropped == 1 {
                "entry"
            } else {
                "entries"
            };
            let _ = writeln!(
                text,
                "  {} {entries} past the panel's limits not kept",
                panel.dropped
            );
        }
    }

    text
}

/// What `muructl requests` shows.
#[derive(Args)]
pub struct RequestsArgs {
    /// Only this application's requests.
    pub app: Option<String>,

    /// List each request in flight, the oldest first, instead of
    /// counting them by worker group.
    #[arg(long)]
    pub list: bool,

    /// How many to list with --list.
    #[arg(long, default_value = "100", value_name = "COUNT")]
    pub limit: u32,

    #[command(flatten)]
    pub socket: SocketArgs,

    #[command(flatten)]
    pub output: JsonArgs,
}

/// Show the requests in flight now (section 11.7): counted by worker
/// group, queued for a slot, holding a thread or on an event loop, with
/// the oldest of each; or listed.
pub async fn requests(args: RequestsArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let list = client
        .list_requests(pb::ListRequestsRequest {
            app: args.app.unwrap_or_default(),
            limit: if args.list { args.limit } else { 0 },
        })
        .await
        .map_err(explain)?
        .into_inner();

    if args.output.json {
        return print_answer(&list);
    }

    if list.in_flight == 0 && list.groups.is_empty() {
        println!("no requests in flight");
        return Ok(());
    }

    let age = |busy: u32, oldest_ms: u64| {
        if busy == 0 {
            "-".to_owned()
        } else {
            muru_dashboard::inspect::age(oldest_ms)
        }
    };

    if !args.list {
        let threads = list.groups.iter().any(|group| group.threads);
        let event_loop = list.groups.iter().any(|group| group.event_loop);
        let mut header = format!(
            "{:<20} {:<12} {:<7} {:<14}",
            "RELEASE", "GROUP", "QUEUED", "QUEUED-OLDEST"
        );

        if threads {
            header += &format!(" {:<8} {:<14}", "THREADS", "THREAD-OLDEST");
        }

        if event_loop {
            header += &format!(" {:<6} {:<12}", "LOOP", "LOOP-OLDEST");
        }

        println!("{}", header.trim_end());

        for group in &list.groups {
            let mut line = format!(
                "{:<20} {:<12} {:<7} {:<14}",
                format!("{}@{}", group.app, group.release),
                group.group,
                group.queued,
                age(group.queued, group.queued_oldest_ms)
            );

            if threads {
                let (busy, oldest) = if group.threads {
                    (
                        format!("{}/{}", group.threads_busy, group.threads_of),
                        age(group.threads_busy, group.threads_oldest_ms),
                    )
                } else {
                    ("-".to_owned(), "-".to_owned())
                };
                line += &format!(" {busy:<8} {oldest:<14}");
            }

            if event_loop {
                let (busy, oldest) = if group.event_loop {
                    (
                        group.event_loop_busy.to_string(),
                        age(group.event_loop_busy, group.event_loop_oldest_ms),
                    )
                } else {
                    ("-".to_owned(), "-".to_owned())
                };
                line += &format!(" {busy:<6} {oldest:<12}");
            }

            println!("{}", line.trim_end());
        }

        return Ok(());
    }

    if list.in_flight == 0 {
        println!("no requests in flight");
        return Ok(());
    }

    println!(
        "{:<8} {:<8} {:<15} {:<7} {:<40} {:<16} REQUEST-ID",
        "AGE", "STATE", "KIND", "WORKER", "REQUEST", "CLIENT"
    );

    for request in &list.requests {
        println!(
            "{:<8} {:<8} {:<15} {:<7} {:<40} {:<16} {}",
            muru_dashboard::inspect::age(request.age_ms),
            request.state,
            request.kind,
            if request.worker < 0 {
                "-".to_owned()
            } else {
                request.worker.to_string()
            },
            format!("{} {}", request.method, request.target),
            request.client,
            request.request_id
        );
    }

    if list.requests.len() < list.in_flight as usize {
        println!(
            "({} of {} in flight shown; --limit shows more)",
            list.requests.len(),
            list.in_flight
        );
    }

    Ok(())
}

/// Re-read the daemon's configuration file.
pub async fn daemon_reload(socket: SocketArgs, json: bool) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let response = client
        .reload_config(pb::ReloadConfigRequest {})
        .await
        .map_err(|status| anyhow::anyhow!("{}", status.message()))?
        .into_inner();

    if json {
        return print_answer(&response);
    }

    println!("muru: {}", response.message);
    Ok(())
}

/// The exit status of `muructl daemon status` when no daemon is
/// running, as for an init script's status.
const NOT_RUNNING: i32 = 3;

/// Show the daemon's status, exiting 3 when none is running. With none
/// running and no socket named, it says where a daemon started with the
/// same options and environment would keep its files.
pub async fn daemon_status(
    socket: SocketArgs,
    files: DaemonFileArgs,
    json: bool,
) -> anyhow::Result<i32> {
    let path = socket.socket()?;
    let locations = daemon_locations(&socket, &files);

    if path == locations.admin_socket() && no_daemon(&socket, &path) {
        let status = pb::Status {
            kind: "stopped".to_owned(),
            socket: path.display().to_string(),
            config: locations.config.path.display().to_string(),
            state_dir: locations.state.path.display().to_string(),
            run_dir: locations.run.path.display().to_string(),
            log_dir: locations.logs.path.display().to_string(),
            cache_dir: locations.cache.path.display().to_string(),
            ..pb::Status::default()
        };

        if json {
            print_answer(&status)?;
        } else {
            println!("daemon   not running");
            print_daemon_files(&status);
        }

        return Ok(NOT_RUNNING);
    }

    show_status(&client::Endpoint::unix(&path), None, false, json).await?;
    Ok(0)
}

/// Show the status of what answers at an admin socket: a daemon or a
/// development run.
pub async fn show_status(
    endpoint: &client::Endpoint,
    app: Option<&str>,
    sizes: bool,
    json: bool,
) -> anyhow::Result<()> {
    let mut client = client::connect(endpoint).await?;
    let status = client
        .get_status(pb::GetStatusRequest {})
        .await?
        .into_inner();

    // Issue 849: a run's or a serve's data directory, and an application's
    // under the daemon when one is named.

    let data = if status.kind == "run" || app.is_some() {
        Some(app_data(endpoint, app.unwrap_or_default(), sizes).await?)
    } else {
        None
    };

    if json {
        #[derive(serde::Serialize)]
        struct WithData<'a> {
            #[serde(flatten)]
            status: &'a pb::Status,
            #[serde(skip_serializing_if = "Option::is_none")]
            data: Option<&'a pb::AppData>,
        }

        return print_answer(&WithData {
            status: &status,
            data: data.as_ref(),
        });
    }

    // A development run says what it runs and from where; it has no
    // state directory, edge process or budget of its own.

    if status.kind == "run" {
        println!("run      pid {} version {}", status.pid, status.version);
        println!("app      {}@{}", status.app, status.release);
        println!("project  {}", status.project);
        println!("serving  {}", status.listeners.join(", "));
        println!("socket   {}", status.socket);

        if !status.dashboard.is_empty() {
            println!("dashboard http://{}/", status.dashboard);
            println!("metrics  http://{}/metrics", status.dashboard);
        }

        println!("run dir  {}", status.run_dir);
        println!("websockets {} open", status.websockets);

        if let Some(limits) = &status.limits {
            print_limits(limits);
        }

        if let Some(data) = &data {
            print!("{}", data_lines(data, false));
        }

        return Ok(());
    }

    match status.name.as_str() {
        "" | "user" | "system" => {
            println!("daemon   pid {} version {}", status.pid, status.version);
        }
        name => println!(
            "daemon   pid {} version {}, instance {name}",
            status.pid, status.version
        ),
    }

    println!("edge     pid {}", status.edge_pid);
    println!("serving  {}", status.listeners.join(", "));

    if !status.metrics.is_empty() {
        println!("metrics  http://{}/metrics", status.metrics);
    }

    if !status.dashboard.is_empty() {
        println!("dashboard http://{}/", status.dashboard);
    }

    print_daemon_files(&status);
    println!(
        "apps     {} ({} releases)",
        status.app_count, status.release_count
    );
    println!("websockets {} open", status.websockets);

    if !status.no_host.is_empty() {
        println!(
            "routing  {} serve with no hosts declared, so every request is answered 421",
            status.no_host
        );
    }

    if !status.budget.is_empty() {
        println!("budget   {}", status.budget);
    }

    if let Some(limits) = &status.limits {
        print_limits(limits);
    }

    if let Some(data) = &data {
        println!("app      {}", data.app);
        print!("{}", data_lines(data, false));
    }

    Ok(())
}

/// A cgroup's limits and the memory guard, as `muructl status` prints
/// them (sections 7.7 and 13.9).
fn print_limits(limits: &pb::CgroupLimits) {
    print!("{}", limits_lines(limits));
}

fn limits_lines(limits: &pb::CgroupLimits) -> String {
    use muru_config::format_size_approx as size;

    let mut text = String::new();

    let cpu = if limits.cpu_quota > 0.0 {
        let quota = format!("{:.2}", limits.cpu_quota);
        let quota = quota.trim_end_matches('0').trim_end_matches('.');
        let noun = if quota == "1" { "CPU" } else { "CPUs" };
        format!("{quota} {noun} from cpu.max")
    } else if limits.cpus > 0 {
        let noun = if limits.cpus == 1 { "CPU" } else { "CPUs" };
        format!(
            "no CPU quota, {} {noun} from cpuset.cpus.effective",
            limits.cpus
        )
    } else {
        "no CPU quota".to_owned()
    };
    text.push_str(&format!("cpu      {cpu}\n"));

    if limits.memory_max == 0 {
        text.push_str("memory   no memory.max\n");
        return text;
    }

    text.push_str(&format!(
        "memory   memory.max {}, {} used, {} left, headroom {}\n",
        size(limits.memory_max),
        size(limits.memory_used),
        size(limits.memory_left),
        size(limits.headroom)
    ));
    text.push_str(&format!(
        "         workers {}, host {}\n",
        size(limits.workers_memory),
        size(limits.host_memory)
    ));

    let mut guard = match limits.guard.as_str() {
        "watching" => "watching".to_owned(),
        state => format!("{state}: {}", limits.guard_note),
    };

    if !limits.last_replaced.is_empty() {
        let seconds = limits.last_replaced_unix_ms / 1000 % 86_400;
        guard.push_str(&format!(
            "; it last replaced {} at {:02}:{:02}:{:02} UTC",
            limits.last_replaced,
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        ));
    }

    text.push_str(&format!("guard    {guard}\n"));
    text
}

/// The daemon's socket, configuration file and directories, as `daemon
/// status` prints them.
fn print_daemon_files(status: &pb::Status) {
    println!("socket   {}", status.socket);
    println!("config   {}", status.config);
    println!("state    {}", status.state_dir);
    println!("run      {}", status.run_dir);
    println!("logs     {}", status.log_dir);
    println!("cache    {}", status.cache_dir);
}

/// Make sure a daemon runs: start one when none answers.
pub async fn up(args: DaemonRunArgs) -> anyhow::Result<()> {
    daemon_start(args).await
}

/// A release's own settings, as `deploy` and `render` both take them,
/// so that the two cannot come to differ.
#[derive(Args)]
pub struct ReleaseArgs {
    /// The application as module:callable, else from the source's [tool.muru] table.
    #[arg(long)]
    pub(crate) entrypoint: Option<String>,

    /// The interface the entrypoint speaks: auto, wsgi or asgi.
    #[arg(long, value_name = "NAME")]
    pub(crate) interface: Option<muru_config::Interface>,

    /// The interpreter: a path, or a version request such as 3.13 that
    /// the daemon resolves with uv (default: the source's .venv, else
    /// its own request).
    #[arg(long)]
    pub(crate) python: Option<PathBuf>,

    /// When the daemon builds the release's environment from uv.lock:
    /// auto (the default) when the source has a lock and no .venv, always
    /// when it has a lock, never.
    #[arg(long, value_name = "MODE")]
    pub(crate) build: Option<String>,

    /// An extra the built environment includes; repeatable.
    #[arg(long = "build-extra", value_name = "NAME")]
    pub(crate) build_extra: Vec<String>,

    /// A dependency group the built environment includes; repeatable.
    #[arg(long = "build-group", value_name = "NAME")]
    pub(crate) build_group: Vec<String>,

    /// Where the release's bytecode goes: redirect (the default, a
    /// directory of its own under the state directory), source (beside
    /// the source, as Python does) or off.
    #[arg(long, value_name = "MODE")]
    pub(crate) bytecode: Option<String>,

    /// Worker processes.
    #[arg(long)]
    pub(crate) processes: Option<u32>,

    /// Application threads per worker.
    #[arg(long)]
    pub(crate) threads: Option<u32>,

    /// Threads for synchronous code called from an ASGI application.
    #[arg(long, value_name = "N")]
    pub(crate) sync_threads: Option<u32>,

    /// Requests an ASGI worker's event loop takes at once.
    #[arg(long, value_name = "N")]
    pub(crate) concurrency: Option<u32>,

    /// The preload mode: auto, true or false.
    #[arg(long, value_name = "MODE")]
    pub(crate) preload: Option<muru_config::Preload>,

    /// The hooks module.
    #[arg(long)]
    pub(crate) hooks: Option<String>,

    /// The entrypoint is a factory returning the application.
    #[arg(long)]
    pub(crate) factory: bool,

    /// Replace a worker after this many requests, with jitter.
    #[arg(long, value_name = "N")]
    pub(crate) max_requests: Option<u64>,

    /// Replace a worker after this long, for example 1h, with jitter.
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    pub(crate) max_lifetime: Option<Duration>,

    /// Workers a group may run over its count while workers are swapped
    /// (default 1): a replacement starts before the old worker stops
    /// while the group is within it, and with 0 the old worker stops
    /// first.
    #[arg(long, value_name = "N")]
    pub(crate) max_surge: Option<u32>,

    /// Workers a group may run under its count while workers are
    /// swapped (default 0).
    #[arg(long, value_name = "N")]
    pub(crate) max_unavailable: Option<u32>,

    /// A host timeout as NAME=DURATION: soft (interrupt a request that
    /// runs this long; off), hard (replace a worker whose request runs
    /// this long; off), hard_drain_grace (10s), shutdown (5s),
    /// websocket_drain_grace (how long a stopping worker keeps its open
    /// WebSockets; 0s), exit_grace (how long a stopping worker has for its
    /// shutdown work; 2s), loop_stall (replace an ASGI worker whose event
    /// loop stalls this long; 10s) or gil_stall (replace a worker in
    /// which no thread has run Python for this long; 30s). Repeatable.
    #[arg(long = "timeout", value_name = "NAME=DURATION", value_parser = parse_host_timeout)]
    pub(crate) timeouts: Vec<(String, Duration)>,

    /// A memory setting as KEY=VALUE: measure (uss, pss or rss),
    /// soft_limit (for example 800MB), soft_for (2m), reclaim_first
    /// (true), hard_limit (1200MB) or check_interval (5s). Repeatable.
    #[arg(long = "memory", value_name = "KEY=VALUE", value_parser = crate::parse_memory_setting)]
    pub(crate) memory: Vec<(String, String)>,

    /// An environment variable for the workers as NAME=VALUE, over the
    /// source's [tool.muru.env] table. Repeatable.
    #[arg(long = "env", value_name = "NAME=VALUE", value_parser = parse_env_setting)]
    pub(crate) env: Vec<(String, String)>,

    /// A secret for the workers as NAME=STORE:KEY, delivered as the
    /// variable, or NAME=STORE:KEY:file for a file named by NAME_FILE.
    /// Repeatable.
    #[arg(long = "secret", value_name = "NAME=STORE:KEY[:file]", value_parser = parse_secret_setting)]
    pub(crate) secrets: Vec<(String, String, String, bool)>,

    /// Run an HTTP server of this profile instead of workers: gunicorn,
    /// uvicorn, granian, mod_wsgi-express, uwsgi or generic, with its
    /// command after `--`.
    #[arg(long, value_name = "PROFILE")]
    pub(crate) server: Option<muru_config::ServerProfile>,

    /// How a managed server's instances bind: unix (the default) or port.
    #[arg(long, value_name = "MODE")]
    pub(crate) bind: Option<muru_config::ServerBind>,

    /// How many instances of a managed server's command run (default 1).
    #[arg(long, value_name = "N")]
    pub(crate) instances: Option<u32>,

    /// The path probed until a managed server's instance answers 2xx or
    /// 3xx; any answer makes it ready without one.
    #[arg(long, value_name = "PATH")]
    pub(crate) readiness: Option<String>,

    /// How long a managed server's instances get to be ready (default 60s).
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    pub(crate) readiness_timeout: Option<Duration>,

    /// The signal that stops a managed server's instance gracefully
    /// (TERM, INT, QUIT, HUP, USR1, USR2 or WINCH); required by the
    /// generic profile, the other profiles know their server's.
    #[arg(long, value_name = "NAME")]
    pub(crate) stop_signal: Option<String>,

    /// A directory a path in an X-Muru-Sendfile response header must lie
    /// in for the file to be served, relative to the source unless
    /// absolute; repeatable (default the table's).
    #[arg(long = "sendfile-root", value_name = "DIR")]
    pub(crate) sendfile_roots: Vec<String>,

    /// A second response header honoured as X-Muru-Sendfile is, such as
    /// X-Sendfile.
    #[arg(long, value_name = "NAME")]
    pub(crate) sendfile_header: Option<String>,

    /// A directory of the application's own files the edge serves under
    /// a path prefix, as PREFIX=DIR, with DIR source:SUBDIR or relative
    /// for a directory in the source, static: or media:, over the
    /// source's [[tool.muru.static]] entries by prefix. Repeatable.
    #[arg(long = "static", value_name = "PREFIX=DIR", value_parser = crate::parse_static_setting)]
    pub(crate) static_dirs: Vec<(String, String)>,

    /// The command of a managed server, after `--`, with `{bind}` where
    /// it takes its address; without `{bind}` the profile adds the
    /// server's own flags.
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<String>,
}

/// An `--initial-processes` value: a count for the release, or
/// `GROUP=N` for one worker group.
fn parse_initial_processes(text: &str) -> Result<(Option<String>, u32), String> {
    let (group, count) = match text.split_once('=') {
        Some((group, count)) if !group.is_empty() => (Some(group.to_owned()), count),
        Some(_) => return Err("give N, or GROUP=N for one worker group".to_owned()),
        None => (None, text),
    };

    match count.parse::<u32>() {
        Ok(count) if count >= 1 => Ok((group, count)),
        _ => Err(format!("{count:?} is not a count of at least 1")),
    }
}

/// What `deploy` takes.
#[derive(Args)]
pub struct DeployArgs {
    /// The application name.
    app: String,

    /// The source directory (default: the current directory).
    source: Option<PathBuf>,

    #[command(flatten)]
    settings: ReleaseArgs,

    /// The release id (default: generated, r1, r2, ...).
    #[arg(long, value_name = "ID")]
    release: Option<String>,

    /// Return as soon as the release is recorded, without waiting for it
    /// to serve.
    #[arg(long)]
    no_wait: bool,

    /// Start the release but leave traffic where it is, for a preview
    /// with muructl preview or a rollout.
    #[arg(long)]
    hold: bool,

    /// With --hold, open an entry link to the release's preview in the
    /// browser once it is ready.
    #[arg(long, requires = "hold", conflicts_with_all = ["no_wait", "dry_run"])]
    open_preview: bool,

    /// Deploy although an overlay freezes the application's deploys;
    /// recorded as an event.
    #[arg(long)]
    override_freeze: bool,

    /// How long the release this one replaces stays warm for an instant
    /// rollback (default 30m; 0 stops it as soon as it has drained).
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    retain: Option<Duration>,

    /// The workers each group of the release this one replaces keeps
    /// while it stays warm (default 1, or 0 for a group whose
    /// min_workers is 0; 0 keeps the host alone).
    #[arg(long, value_name = "N")]
    retain_workers: Option<u32>,

    /// The workers the release starts with, where that is fewer than
    /// its processes: the count the node's budget checks the deploy at,
    /// and the count the release keeps while it takes no traffic. Once
    /// it serves it takes its processes, or what its scaling policy
    /// says. GROUP=N gives one worker group's; repeatable.
    #[arg(long = "initial-processes", value_name = "N|GROUP=N", value_parser = parse_initial_processes)]
    initial_processes: Vec<(Option<String>, u32)>,

    /// Report what the deploy would do, with where each of the release's
    /// settings would come from, and change nothing.
    #[arg(long)]
    dry_run: bool,

    /// Record KEY=VALUE with the release, such as
    /// description=TEXT or url=URL; repeatable. Over what is filled from
    /// git, and KEY= leaves a filled key out.
    #[arg(long = "metadata", value_name = "KEY=VALUE", value_parser = crate::release_metadata::parse_flag)]
    metadata: Vec<(String, String)>,

    /// Fill no commit, ref or dirty flag from the git checkout the
    /// source directory is in.
    #[arg(long)]
    no_git_metadata: bool,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    output: JsonArgs,
}

impl DeployArgs {
    /// The application deployed.
    pub fn app(&self) -> &str {
        &self.app
    }

    /// The source directory, whose muru.local.toml says which instance the
    /// deploy goes to.
    pub fn source(&self) -> PathBuf {
        self.source
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
    }
}

/// Parse NAME=VALUE for --env.
fn parse_env_setting(text: &str) -> Result<(String, String), String> {
    let (name, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not NAME=VALUE"))?;

    if !muru_controller::model::valid_env_name(name) {
        return Err(format!("{name:?} is not a variable name"));
    }

    Ok((name.to_owned(), value.to_owned()))
}

/// Parse NAME=STORE:KEY[:file] for --secret.
fn parse_secret_setting(text: &str) -> Result<(String, String, String, bool), String> {
    let (name, reference) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not NAME=STORE:KEY"))?;

    if !muru_controller::model::valid_env_name(name) {
        return Err(format!("{name:?} is not a variable name"));
    }

    let (store, rest) = reference
        .split_once(':')
        .ok_or_else(|| format!("{reference:?} is not STORE:KEY"))?;

    let (key, as_file) = match rest.rsplit_once(':') {
        Some((key, "file")) => (key, true),
        Some((key, "env")) => (key, false),
        _ => (rest, false),
    };

    if store.is_empty() || key.is_empty() {
        return Err(format!("{reference:?} is not STORE:KEY"));
    }

    Ok((name.to_owned(), store.to_owned(), key.to_owned(), as_file))
}

/// The secret subcommands (section 10.8): the file stores the daemon's
/// configuration declares, managed without the daemon.
#[derive(Subcommand)]
pub enum SecretCommand {
    /// Store a value read from standard input under a key.
    Set {
        /// The file store, as [secrets.store.NAME] names it.
        store: String,

        /// The key.
        key: String,

        #[command(flatten)]
        locations: LocationArgs,
    },

    /// List the keys of a file store; values are never shown.
    List {
        /// The file store.
        store: String,

        #[command(flatten)]
        locations: LocationArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Remove a key from a file store.
    Remove {
        /// The file store.
        store: String,

        /// The key.
        key: String,

        #[command(flatten)]
        locations: LocationArgs,
    },
}

/// Manage a file secret store.
pub async fn secret(command: SecretCommand) -> anyhow::Result<()> {
    let (store, locations) = match &command {
        SecretCommand::Set {
            store, locations, ..
        }
        | SecretCommand::List {
            store, locations, ..
        }
        | SecretCommand::Remove {
            store, locations, ..
        } => (store.clone(), locations.resolve()?),
    };

    require_local("secret", locations.instance.as_deref())?;

    let document = read_config(&locations.config.path)?;
    let stores = muru_controller::secrets::Store::from_document(
        &document,
        &locations.state.path.join("secrets"),
    )?;

    let directory = match stores.iter().find(|candidate| candidate.name == store) {
        Some(muru_controller::secrets::Store {
            kind: muru_controller::secrets::Kind::File(directory),
            ..
        }) => directory.clone(),
        Some(_) => anyhow::bail!("secret store {store} is not a file store"),
        None => anyhow::bail!(
            "no secret store named {store} in {}",
            locations.config.path.display()
        ),
    };

    match command {
        SecretCommand::Set { key, .. } => {
            anyhow::ensure!(
                !key.is_empty() && !key.contains('/') && !key.starts_with('.'),
                "{key:?} is not a valid key"
            );
            let mut value = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut value)
                .context("reading the value from standard input")?;
            let value = value.trim_end_matches(['\n', '\r']);
            anyhow::ensure!(!value.is_empty(), "no value was given on standard input");
            std::fs::create_dir_all(&directory)
                .with_context(|| format!("creating {}", directory.display()))?;
            std::fs::set_permissions(
                &directory,
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )?;
            let path = directory.join(&key);
            let _ = std::fs::remove_file(&path);
            {
                use std::io::Write as _;
                use std::os::unix::fs::OpenOptionsExt as _;

                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .with_context(|| format!("writing {}", path.display()))?;
                file.write_all(value.as_bytes())?;
            }
            println!("muru: secret {store}/{key} set");
        }
        SecretCommand::List { output, .. } => {
            let mut keys: Vec<String> = match std::fs::read_dir(&directory) {
                Ok(entries) => entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().into_string().ok())
                    .filter(|name| !name.starts_with('.'))
                    .collect(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => {
                    return Err(error).context(format!("reading {}", directory.display()));
                }
            };

            keys.sort();

            // The store is read here rather than asked of the daemon, so
            // its JSON is the store's name and keys.

            if output.json {
                return print_answer(&serde_json::json!({ "store": store, "keys": keys }));
            }

            if keys.is_empty() {
                println!("no secrets in {store}");
            }

            for key in keys {
                println!("{key}");
            }
        }
        SecretCommand::Remove { key, .. } => {
            let path = directory.join(&key);
            std::fs::remove_file(&path)
                .with_context(|| format!("secret {store}/{key} does not exist"))?;
            println!("muru: secret {store}/{key} removed");
        }
    }

    Ok(())
}

/// The last value given for a memory setting.
fn memory_setting(settings: &[(String, String)], key: &str) -> Option<String> {
    settings
        .iter()
        .rev()
        .find(|(given, _)| given == key)
        .map(|(_, value)| value.clone())
}

/// The last value given for a named timeout, in milliseconds.
fn timeout_ms(timeouts: &[(String, Duration)], name: &str) -> Option<u64> {
    timeouts
        .iter()
        .rev()
        .find(|(given, _)| given == name)
        .map(|(_, duration)| duration.as_millis() as u64)
}

/// Parse NAME=DURATION for deploy --timeout.
fn parse_host_timeout(text: &str) -> Result<(String, Duration), String> {
    let (name, duration) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not NAME=DURATION, for example hard=60s"))?;
    let name = name.trim();

    if ![
        "soft",
        "hard",
        "hard_drain_grace",
        "shutdown",
        "websocket_drain_grace",
        "exit_grace",
        "loop_stall",
        "gil_stall",
    ]
    .contains(&name)
    {
        return Err(format!(
            "{name:?} is not a host timeout name: soft, hard, hard_drain_grace, shutdown, \
             websocket_drain_grace, exit_grace, loop_stall or gil_stall"
        ));
    }

    Ok((name.to_owned(), muru_config::parse_duration(duration)?))
}

/// An interpreter as the daemon should see it: a path with a directory in
/// it is made absolute against `base` (else the working directory), since
/// the daemon runs elsewhere; a bare command name is left for the PATH.
fn absolute_interpreter(python: &Path, base: Option<&Path>) -> String {
    if python.is_absolute() || python.components().count() < 2 {
        return python.display().to_string();
    }

    let base = base
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    base.join(python).display().to_string()
}

impl ReleaseArgs {
    /// What cannot be asked for together.
    pub(crate) fn check(&self) -> anyhow::Result<()> {
        if self.server.is_none() && !self.command.is_empty() {
            anyhow::bail!("a command after -- needs --server PROFILE");
        }

        Ok(())
    }

    /// These settings as the deploy request that carries them, for the
    /// application and its source, with the interpreter as the caller
    /// has it written. What says how the deploy is carried out is left
    /// for the caller to set. No flag sets the handoff or the build's
    /// source_builds and compile_bytecode: a deploy takes them from the
    /// source's settings, and only a release's table in an applied file
    /// gives them.
    pub(crate) fn request(self, app: &str, source: &str, python: String) -> pb::DeployRequest {
        pb::DeployRequest {
            metadata: std::collections::BTreeMap::new(),
            initial_processes: None,
            group_initial_processes: std::collections::BTreeMap::new(),
            app: app.to_owned(),
            source: source.to_owned(),
            kind: self.server.map(|_| "http".to_owned()).unwrap_or_default(),
            profile: self
                .server
                .map(|profile| profile.as_str().to_owned())
                .unwrap_or_default(),
            command: self.command.clone(),
            bind: self
                .bind
                .map(|bind| bind.as_str().to_owned())
                .unwrap_or_default(),
            instances: self.instances,
            readiness_path: self.readiness.unwrap_or_default(),
            readiness_timeout_ms: self
                .readiness_timeout
                .map(|timeout| timeout.as_millis() as u64),
            stop_signal: self.stop_signal.unwrap_or_default(),
            entrypoint: self.entrypoint.unwrap_or_default(),
            interface: self
                .interface
                .map(|value| value.as_str().to_owned())
                .unwrap_or_default(),
            python,
            build_when: self.build.unwrap_or_default(),
            build_extras: self.build_extra,
            build_groups: self.build_group,
            bytecode: self.bytecode.unwrap_or_default(),
            handoff: String::new(),
            build_source_builds: None,
            build_compile_bytecode: None,
            processes: self.processes,
            threads: self.threads,
            sync_threads: self.sync_threads,
            concurrency: self.concurrency,
            preload: self
                .preload
                .map(|value| value.as_str().to_owned())
                .unwrap_or_default(),
            hooks: self.hooks.unwrap_or_default(),
            factory: self.factory.then_some(true),
            factory_phase: String::new(),
            release: String::new(),
            wait: true,
            hold: false,
            override_freeze: false,
            reflection: None,
            grpc_web_origins: Vec::new(),
            retain_ms: None,
            retain_workers: None,
            max_requests: self.max_requests,
            max_lifetime_ms: self
                .max_lifetime
                .map(|lifetime| lifetime.as_millis() as u64),
            max_surge: self.max_surge,
            max_unavailable: self.max_unavailable,
            soft_timeout_ms: timeout_ms(&self.timeouts, "soft"),
            accel_redirect: None,
            cgi_local_redirect: None,
            sendfile_roots: self.sendfile_roots,
            sendfile_header: self.sendfile_header,
            static_dirs: self
                .static_dirs
                .iter()
                .map(|(path, dir)| pb::StaticDir {
                    path: path.clone(),
                    dir: dir.clone(),
                })
                .collect(),
            hard_timeout_ms: timeout_ms(&self.timeouts, "hard"),
            hard_drain_grace_ms: timeout_ms(&self.timeouts, "hard_drain_grace"),
            shutdown_timeout_ms: timeout_ms(&self.timeouts, "shutdown"),
            websocket_drain_grace_ms: timeout_ms(&self.timeouts, "websocket_drain_grace"),
            exit_grace_ms: timeout_ms(&self.timeouts, "exit_grace"),
            loop_stall_timeout_ms: timeout_ms(&self.timeouts, "loop_stall"),
            gil_stall_timeout_ms: timeout_ms(&self.timeouts, "gil_stall"),
            memory_measure: memory_setting(&self.memory, "measure").unwrap_or_default(),
            memory_soft_limit: memory_setting(&self.memory, "soft_limit")
                .and_then(|text| muru_config::parse_size(&text).ok()),
            memory_soft_for_ms: memory_setting(&self.memory, "soft_for")
                .and_then(|text| muru_config::parse_duration(&text).ok())
                .map(|duration| duration.as_millis() as u64),
            memory_reclaim_first: memory_setting(&self.memory, "reclaim_first")
                .map(|text| text == "true"),
            memory_hard_limit: memory_setting(&self.memory, "hard_limit")
                .and_then(|text| muru_config::parse_size(&text).ok()),
            memory_check_interval_ms: memory_setting(&self.memory, "check_interval")
                .and_then(|text| muru_config::parse_duration(&text).ok())
                .map(|duration| duration.as_millis() as u64),
            env: self
                .env
                .iter()
                .map(|(name, value)| pb::EnvEntry {
                    name: name.clone(),
                    value: value.clone(),
                    ..Default::default()
                })
                .chain(
                    self.secrets
                        .iter()
                        .map(|(name, store, key, as_file)| pb::EnvEntry {
                            name: name.clone(),
                            secret_store: store.clone(),
                            secret_key: key.clone(),
                            as_file: *as_file,
                            ..Default::default()
                        }),
                )
                .collect(),
        }
    }
}

/// Deploy a release, or with --dry-run report what deploying it would do.
pub async fn deploy(args: DeployArgs) -> anyhow::Result<i32> {
    let source = args
        .source
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let source = source
        .canonicalize()
        .with_context(|| format!("{} does not exist", source.display()))?;

    if let Some(note) = crate::passed_over_note(&source) {
        eprintln!("muru: {note}");
    }

    args.settings.check()?;

    // Section 9.14: the commit and ref the source is at, under what the
    // flags say, checked here before anything is sent.

    let filled = if args.no_git_metadata {
        crate::release_metadata::Filled::default()
    } else {
        crate::release_metadata::from_git(&source).await
    };

    if let Some(why) = &filled.failure {
        eprintln!("muru: no commit or ref recorded from git: {why}");
    }

    let metadata = muru_config::check_metadata(&crate::release_metadata::merged(
        filled.metadata,
        &args.metadata,
    ))
    .map_err(anyhow::Error::msg)?;

    let endpoint = args.socket.endpoint()?;
    let mut client = client::connect(&endpoint).await?;
    let python = args
        .settings
        .python
        .as_deref()
        .map(|path| absolute_interpreter(path, None))
        .unwrap_or_default();

    let request = pb::DeployRequest {
        metadata,
        release: args.release.unwrap_or_default(),
        wait: !args.no_wait,
        hold: args.hold,
        override_freeze: args.override_freeze,
        retain_ms: args.retain.map(|retain| retain.as_millis() as u64),
        retain_workers: args.retain_workers,
        initial_processes: args
            .initial_processes
            .iter()
            .rev()
            .find(|(group, _)| group.is_none())
            .map(|(_, count)| *count),
        group_initial_processes: args
            .initial_processes
            .iter()
            .filter_map(|(group, count)| group.clone().map(|group| (group, *count)))
            .collect(),
        ..args
            .settings
            .request(&args.app, &source.display().to_string(), python)
    };

    if args.dry_run {
        let plan = client
            .plan_deploy(request)
            .await
            .map_err(explain)?
            .into_inner();

        if args.output.json {
            print_answer(&plan)?;
        } else {
            print_deploy_plan(&plan);
        }

        return Ok(i32::from(!plan.refusals.is_empty()));
    }

    let release = client.deploy(request).await.map_err(explain)?.into_inner();

    if args.output.json {
        print_answer(&release)?;
        return Ok(0);
    }

    if !release.notice.is_empty() {
        eprintln!("muru: warning: {}", release.notice);
    }

    if args.no_wait {
        println!(
            "muru: release {}@{} recorded ({})",
            release.app, release.id, release.state
        );
    } else {
        println!(
            "muru: release {}@{} is {}{}{}",
            release.app,
            release.id,
            release.state,
            if release.serving {
                " and serving"
            } else if args.hold {
                " and held: not serving"
            } else {
                ""
            },
            muru_config::metadata_origin(&release.metadata)
                .map(|origin| format!(", {origin}"))
                .unwrap_or_default()
        );
    }

    // A held release is there to be tried: say how, and with
    // --open-preview do it. The preview's secret is never printed here.

    if args.hold && !args.no_wait && !release.serving {
        let name = format!("{}@{}", release.app, release.id);

        if !args.open_preview {
            eprintln!("muru: muructl preview {name} opens a preview of it");
        } else if let Err(error) =
            crate::preview::open_for(&endpoint, &release.app, &release.id, None, false).await
        {
            eprintln!("muru: no preview link: {error:#}; muructl preview {name} gives one");
        } else {
            // The browser is given a moment to start before muructl exits.

            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    Ok(0)
}

/// A release's metadata as Muru's views list it (section 9.14): the
/// well-known keys first, in their order, then the others by key.
pub(crate) fn metadata_in_order(
    metadata: &std::collections::BTreeMap<String, String>,
) -> Vec<(&str, &str)> {
    let known = muru_config::METADATA_KEYS
        .iter()
        .filter_map(|key| metadata.get_key_value(*key));
    let others = metadata
        .iter()
        .filter(|(key, _)| !muru_config::METADATA_KEYS.contains(&key.as_str()));

    known
        .chain(others)
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect()
}

/// A deploy's plan as people read it: the release, each setting with
/// the layer it would come from, and what happens around it.
fn print_deploy_plan(plan: &pb::DeployPlan) {
    if !plan.refusals.is_empty() {
        println!("muru: the deploy would be refused:");

        for refusal in &plan.refusals {
            println!("  {refusal}");
        }

        return;
    }

    println!(
        "muru: plan (nothing changed): release {}@{}, {}",
        plan.app, plan.release, plan.description
    );

    if plan.new_app {
        println!("  the application {} would be created", plan.app);
    }

    for (key, value) in metadata_in_order(&plan.metadata) {
        println!("  metadata {key} = {value}");
    }

    if !plan.settings.is_empty() {
        let lines: Vec<(String, &str)> = plan
            .settings
            .iter()
            .map(|setting| {
                (
                    format!("{} = {}", setting.key, setting.value),
                    setting.layer.as_str(),
                )
            })
            .collect();
        let width = lines
            .iter()
            .map(|(line, _)| line.len())
            .filter(|length| *length <= 44)
            .max()
            .unwrap_or(0);

        for (line, layer) in lines {
            println!("  {line:<width$}  ({layer})");
        }
    }

    for entry in &plan.env {
        match (entry.secret.as_str(), entry.as_file) {
            ("", _) => println!("  env {} ({})", entry.name, entry.layer),
            (secret, false) => {
                println!("  secret {} = {secret} ({})", entry.name, entry.layer);
            }
            (secret, true) => println!(
                "  secret {} = {secret} as a file ({})",
                entry.name, entry.layer
            ),
        }
    }

    if plan.builds {
        println!("  an environment would be built from the source's uv.lock");
    }

    match (plan.hold, plan.serving.is_empty()) {
        (true, true) => println!(
            "  the release would start held, and {} would serve nothing until a rollout",
            plan.app
        ),
        (true, false) => println!(
            "  the release would start held, and {}@{} would go on serving",
            plan.app, plan.serving
        ),
        (false, true) => println!("  the release would serve once it is ready"),
        (false, false) if plan.retain_ms == 0 => println!(
            "  the release would take over from {}@{}, which would stop once it has drained",
            plan.app, plan.serving
        ),
        (false, false) => println!(
            "  the release would take over from {}@{}, which would stay warm for {} with {}",
            plan.app,
            plan.serving,
            muru_config::format_duration(Duration::from_millis(plan.retain_ms)),
            plan.retained
        ),
    }
}

/// The rollout subcommands (section 9.3).
#[derive(Subcommand)]
pub enum RolloutCommand {
    /// Shift the application's traffic to a release step by step, each
    /// step judged by gates against the release that serves; the last
    /// step passing promotes the release, a failing gate rolls back.
    Canary {
        /// The candidate, as app@release.
        release: String,

        /// The steps, as percentages of the application's requests.
        #[arg(long, value_delimiter = ',', default_value = "10,50")]
        steps: Vec<u32>,

        /// How long each step holds before its gates decide.
        #[arg(long, value_name = "DURATION", default_value = "2m", value_parser = muru_config::parse_duration)]
        hold: Duration,

        /// The 5xx rate the candidate may not exceed, as a percentage.
        #[arg(long = "max-error-rate", value_name = "PERCENT", default_value = "1")]
        max_error_rate: f64,

        /// The 95th percentile request time the candidate may not exceed.
        #[arg(long = "max-p95", value_name = "DURATION", value_parser = muru_config::parse_duration)]
        max_p95: Option<Duration>,

        /// The 99th percentile request time the candidate may not exceed.
        #[arg(long = "max-p99", value_name = "DURATION", value_parser = muru_config::parse_duration)]
        max_p99: Option<Duration>,

        /// Worker crashes and timeout kills the candidate may have in a
        /// step; one more rolls back at once.
        #[arg(long = "max-worker-faults", value_name = "COUNT", default_value = "0")]
        max_worker_faults: u64,

        /// The 95th percentile wait for a worker thread the candidate
        /// may not exceed.
        #[arg(long = "max-queue-wait", value_name = "DURATION", value_parser = muru_config::parse_duration)]
        max_queue_wait: Option<Duration>,

        /// A URL requested through the edge from the candidate and the
        /// baseline every few seconds, by preview; one failing on the
        /// candidate alone rolls back. Repeatable.
        #[arg(long = "probe", value_name = "URL")]
        probes: Vec<String>,

        /// Requests the candidate must serve in a step before the gates
        /// judge it.
        #[arg(long = "min-requests", value_name = "COUNT", default_value = "100")]
        min_requests: u64,

        /// Wait for `rollout resume` before each step after the first.
        #[arg(long)]
        approve_each_step: bool,

        /// How the two releases are sized: own, each at its own size
        /// (the default), or shared, the two sharing the worker count
        /// the baseline has when the rollout starts, each sized by its
        /// share of the traffic, for a node with no room for two full
        /// copies.
        #[arg(long, value_name = "MODE", default_value = "own", value_parser = ["own", "shared"])]
        capacity: String,

        /// Stay until the rollout is promoted or rolled back.
        #[arg(long)]
        wait: bool,

        /// Go ahead although an overlay freezes the application's
        /// deploys; recorded as an event.
        #[arg(long)]
        override_freeze: bool,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Move all of the application's traffic to a release now and drain
    /// the one that served (blue/green).
    Bluegreen {
        /// The release, as app@release.
        release: String,

        /// Go ahead although an overlay freezes the application's
        /// deploys; recorded as an event.
        #[arg(long)]
        override_freeze: bool,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Show the rollouts, of one application or of all.
    Status {
        /// The application.
        app: Option<String>,

        /// Stay until the application's rollout is promoted or rolled
        /// back.
        #[arg(long)]
        wait: bool,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Hold the application's rollout at its current step.
    Pause {
        /// The application.
        app: String,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Continue a paused rollout, or approve its next step.
    Resume {
        /// The application.
        app: String,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Give the candidate all the traffic now and end the rollout.
    Promote {
        /// The application.
        app: String,

        /// Go ahead although an overlay freezes the application's
        /// deploys; recorded as an event.
        #[arg(long)]
        override_freeze: bool,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Take the candidate out of traffic now and end the rollout.
    Abort {
        /// The application.
        app: String,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// What `muructl scale` takes: a group's count to pin or release, or
/// `scale status` for the autoscaler's state (sections 7.3 and 7.4).
#[derive(Args, Clone)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
#[command(group = clap::ArgGroup::new("temporary").args(["min_workers", "cap_workers"]))]
pub struct ScaleArgs {
    #[command(subcommand)]
    pub command: Option<ScaleCommand>,

    /// The release as app@release, or an application for its serving
    /// release.
    #[arg(value_name = "APP[@RELEASE]", required = true)]
    target: Option<String>,

    /// The worker group.
    #[arg(long, default_value = "default", value_name = "NAME")]
    group: String,

    /// Pin the group to this many workers; the autoscaler stands aside
    /// until --unpin.
    #[arg(long, value_name = "COUNT", conflicts_with = "unpin")]
    processes: Option<u32>,

    /// Let the autoscaler decide the group's count again.
    #[arg(long)]
    unpin: bool,

    /// Keep at least this many workers until --until or for --for, a
    /// temporary schedule over the application.
    #[arg(long = "min", value_name = "COUNT", conflicts_with_all = ["processes", "unpin"])]
    min_workers: Option<u32>,

    /// With --min, allow up to this many workers meanwhile.
    #[arg(long = "max", value_name = "COUNT", requires = "min_workers")]
    max_workers: Option<u32>,

    /// Allow at most this many workers until --until or for --for,
    /// whatever the policy, a floor and the autoscaler say; the excess
    /// drains at once.
    #[arg(long = "cap", value_name = "COUNT", conflicts_with_all = ["processes", "unpin", "min_workers"])]
    cap_workers: Option<u32>,

    /// With --cap, return at once rather than waiting until the group is
    /// at or under the cap.
    #[arg(long, requires = "cap_workers")]
    no_wait: bool,

    /// When the floor or the cap ends, such as 2026-11-30T23:59 or an
    /// RFC 3339 time.
    #[arg(
        long,
        value_name = "TIME",
        requires = "temporary",
        conflicts_with = "duration"
    )]
    until: Option<String>,

    /// How long the floor or the cap lasts, such as 4h.
    #[arg(long = "for", value_name = "DURATION", value_parser = muru_config::parse_duration, requires = "temporary")]
    duration: Option<Duration>,

    /// The time zone --until is in; the local zone without it.
    #[arg(long, value_name = "ZONE", requires = "until")]
    timezone: Option<String>,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// The `muructl scale` subcommands.
#[derive(Subcommand, Clone)]
pub enum ScaleCommand {
    /// Show each worker group's policy, workers, signals, last decision
    /// and churn.
    Status {
        /// The release as app@release, or an application for its serving
        /// release; the one application of `muructl serve` when left out.
        #[arg(value_name = "APP[@RELEASE]")]
        target: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// What `restart` takes: one worker of a release, by index or pid,
/// gracefully or at once, or every worker in turn.
#[derive(Args)]
pub struct RestartArgs {
    /// The release as app@release, or an application for its current
    /// release; the one application of `muructl run` or `muructl serve`
    /// when left out.
    #[arg(value_name = "APP[@RELEASE]")]
    target: Option<String>,

    /// The worker to restart, as `debug status` and the access log name
    /// it: its index within its group, as GROUP/N outside the default
    /// group.
    #[arg(long, value_name = "[GROUP/]N", conflicts_with_all = ["pid", "all"])]
    worker: Option<WorkerName>,

    /// The worker to restart, by process id.
    #[arg(long, value_name = "PID", conflicts_with = "all")]
    pid: Option<u32>,

    /// Every worker of the release, each in its turn: as many at once
    /// as the max_surge and max_unavailable of its group allow, one at a
    /// time by default, so capacity holds. The release's host takes the
    /// turns, so the restart carries on if the daemon is stopped.
    #[arg(long, required_unless_present_any = ["worker", "pid", "group", "cancel"])]
    all: bool,

    /// Every worker of one worker group, each in its turn.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["worker", "pid"])]
    group: Option<String>,

    /// End a restart of every worker that is under way: the workers not
    /// yet replaced stay as they are.
    #[arg(long, conflicts_with_all = ["worker", "pid", "all", "group", "force", "no_wait"])]
    cancel: bool,

    /// Take the worker out of routing and kill it at once, its stacks
    /// written to the log; the requests it is serving fail, and a
    /// debugger session in it ends. For one worker.
    #[arg(long, conflicts_with_all = ["all", "group"])]
    force: bool,

    /// Return once the restart is under way, without waiting for the
    /// replacement to serve and the old worker to stop.
    #[arg(long)]
    no_wait: bool,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// How long `restart` waits for a replacement to serve and the old
/// worker to go: a start, the routing change and the stop budget, with
/// room for a slow start.
const RESTART_WAIT: Duration = Duration::from_secs(120);

/// How often `restart` looks at the host's status while it waits.
const RESTART_POLL: Duration = Duration::from_millis(250);

/// Replace one worker of a release, or each in turn, with a fresh one.
pub async fn restart(args: RestartArgs) -> anyhow::Result<()> {
    if args.cancel || args.all || args.group.is_some() {
        return restart_release(args).await;
    }

    let status = ask(
        &args.socket,
        args.target.as_deref().unwrap_or_default(),
        "status",
        None,
        serde_json::json!({}),
    )
    .await?;
    let serving: Vec<(WorkerName, u32, String)> = status
        .workers
        .iter()
        .filter_map(|answer| {
            let result = parse_result(answer)?;
            let serving = result["ready"].as_bool() == Some(true)
                && result["replaced"].as_bool() != Some(true);
            serving.then(|| {
                (
                    answered_name(answer),
                    answer.pid,
                    result["group"].as_str().unwrap_or("default").to_owned(),
                )
            })
        })
        .collect();

    // The release as the lines name it: app@release, or the release alone
    // for the one application of a run, which named none.

    let release = match args
        .target
        .as_deref()
        .and_then(|target| target.split('@').next())
    {
        Some(app) => format!("{app}@{}", status.release),
        None => status.release.clone(),
    };

    let chosen: Vec<(WorkerName, u32)> = if let Some(pid) = args.pid {
        serving
            .iter()
            .find(|(_, candidate, _)| *candidate == pid)
            .map(|(name, pid, _)| vec![(name.clone(), *pid)])
            .ok_or_else(|| anyhow::anyhow!("{release} has no serving worker with pid {pid}"))?
    } else {
        let wanted = args
            .worker
            .as_ref()
            .expect("clap requires --worker or --pid without --all");
        serving
            .iter()
            .find(|(name, _, _)| name == wanted)
            .map(|(name, pid, _)| vec![(name.clone(), *pid)])
            .ok_or_else(|| anyhow::anyhow!("{release} has no serving worker {wanted}"))?
    };

    // With --json, one object for each worker replaced, printed together
    // at the end.

    let mut replaced = Vec::new();

    for (name, pid) in chosen {
        let response = ask(
            &args.socket,
            args.target.as_deref().unwrap_or_default(),
            "restart",
            Some(&name),
            serde_json::json!({ "force": args.force }),
        )
        .await?;
        let answer = &response.workers[0];

        if !answer.ok {
            anyhow::bail!("worker {name} (pid {pid}): {}", answer.error);
        }

        let new_pid = parse_result(answer)
            .and_then(|result| result["new_pid"].as_u64())
            .unwrap_or_default();
        let how = if args.force { ", killed at once" } else { "" };

        // Section 7.8: a worker that stops first has no successor yet.

        if !args.output.json && new_pid == 0 {
            println!(
                "muru: worker {name} (pid {pid}) is stopping, and is replaced once it has stopped{how}"
            );
        } else if !args.output.json {
            println!("muru: worker {name} (pid {pid}) is being replaced by pid {new_pid}{how}");
        }

        let served_by = match args.no_wait {
            true => None,
            false => Some(wait_restarted(&args, &name, pid).await?),
        };

        match (args.output.json, served_by) {
            (true, _) => replaced.push(serde_json::json!({
                "index": name.index,
                "group": name.group,
                "pid": pid,
                "new_pid": new_pid,
                "forced": args.force,
                "served_by": served_by,
            })),
            (false, Some(served_by)) => {
                println!("muru: worker {name} is served by pid {served_by}; pid {pid} has stopped");
            }
            (false, None) => {}
        }
    }

    if args.output.json {
        return print_answer(&serde_json::json!({ "release": release, "workers": replaced }));
    }

    Ok(())
}

/// `restart --all`, `--group` and `--cancel`: every worker of a release,
/// or of one group, replaced in turn by one call of the admin API, which
/// the release's host carries out, or such a restart cancelled.
async fn restart_release(args: RestartArgs) -> anyhow::Result<()> {
    let target = args.target.as_deref().unwrap_or_default();

    let (app, release) = match target {
        "" => ("", ""),
        target => split_target(target)?,
    };

    let mut client = client::connect(&args.socket.endpoint()?).await?;

    if args.cancel {
        let answer = client
            .cancel_restart(pb::CancelRestartRequest {
                app: app.to_owned(),
                release: release.to_owned(),
            })
            .await
            .map_err(explain)?
            .into_inner();

        if args.output.json {
            return print_answer(&answer);
        }

        println!(
            "muru: the restart of {} is cancelled: {} left as {} {}",
            release_name(&answer.app, &answer.release),
            counted(answer.workers.len(), "worker", "workers"),
            if answer.workers.len() == 1 {
                "it"
            } else {
                "they"
            },
            if answer.workers.len() == 1 {
                "was"
            } else {
                "were"
            },
        );
        return Ok(());
    }

    let answer = client
        .restart_release(pb::RestartReleaseRequest {
            app: app.to_owned(),
            release: release.to_owned(),
            group: args.group.clone().unwrap_or_default(),
            wait: !args.no_wait,
            wait_timeout_ms: 0,
        })
        .await
        .map_err(explain)?
        .into_inner();
    let name = release_name(&answer.app, &answer.release);
    let scope = args
        .group
        .as_ref()
        .map(|group| format!(" of the group {group}"))
        .unwrap_or_default();

    if answer.workers.is_empty() {
        anyhow::bail!("{name} has no worker{scope} to restart");
    }

    if args.output.json {
        print_answer(&answer)?;
    } else {
        println!(
            "muru: {}{scope} of {name} marked to be replaced, each in its turn",
            counted(answer.workers.len(), "worker", "workers"),
        );
    }

    match (&answer.restart, args.no_wait) {
        (Some(restart), false) => anyhow::bail!(
            "the restart of {name} is still under way: {} of {} left; muructl release show \
             says how far it has got, and muructl restart --cancel ends it",
            counted(restart.left as usize, "worker", "workers"),
            restart.total
        ),
        (None, false) if !args.output.json => {
            println!("muru: the restart of {name} has finished");
        }
        _ => {}
    }

    Ok(())
}

/// A release as a line names it: app@release, or the release alone for
/// the one application of a run that named none.
fn release_name(app: &str, release: &str) -> String {
    if app.is_empty() {
        release.to_owned()
    } else {
        format!("{app}@{release}")
    }
}

/// Wait until the worker at `index` is one other than `old_pid`, ready,
/// with the old one gone, and return the pid that serves.
async fn wait_restarted(
    args: &RestartArgs,
    name: &WorkerName,
    old_pid: u32,
) -> anyhow::Result<u32> {
    let deadline = Instant::now() + RESTART_WAIT;

    loop {
        tokio::time::sleep(RESTART_POLL).await;
        let status = ask(
            &args.socket,
            args.target.as_deref().unwrap_or_default(),
            "status",
            Some(name),
            serde_json::json!({}),
        )
        .await;

        // The host's status names every worker at the index, the old one
        // and its replacement while both exist.

        if let Ok(status) = status {
            let old_gone = status.workers.iter().all(|answer| answer.pid != old_pid);
            let ready = status.workers.iter().find(|answer| {
                answer.pid != old_pid
                    && parse_result(answer)
                        .is_some_and(|result| result["ready"].as_bool() == Some(true))
            });

            if let (true, Some(answer)) = (old_gone, ready) {
                return Ok(answer.pid);
            }
        }

        anyhow::ensure!(
            Instant::now() < deadline,
            "worker {name} was not replaced within {} seconds; muructl debug status shows where it is",
            RESTART_WAIT.as_secs()
        );
    }
}

/// Pin or release a group's count, or show the autoscaler's state.
pub async fn scale(args: ScaleArgs) -> anyhow::Result<()> {
    if let Some(ScaleCommand::Status {
        target,
        socket,
        output,
    }) = args.command
    {
        let (app, release) = match target.as_deref() {
            Some(target) => split_target(target)?,
            None => ("", ""),
        };

        let mut client = client::connect(&socket.endpoint()?).await?;
        let status = client
            .get_scale_status(pb::GetScaleStatusRequest {
                app: app.to_owned(),
                release: release.to_owned(),
            })
            .await
            .map_err(explain)?
            .into_inner();

        if output.json {
            return print_answer(&status);
        }

        print_scale_status(&status);
        return Ok(());
    }

    let target = args.target.unwrap_or_default();
    let (app, release) = split_target(&target)?;

    if args.processes.is_none()
        && !args.unpin
        && args.min_workers.is_none()
        && args.cap_workers.is_none()
    {
        anyhow::bail!(
            "give --processes COUNT to pin the group, --unpin, or --min COUNT or --cap COUNT with --until or --for; `scale status` shows the state"
        );
    }

    let until_unix_ms = match (&args.until, args.duration) {
        (Some(text), _) => muru_config::parse_moment(text, args.timezone.as_deref())
            .map_err(|error| anyhow::anyhow!("--until: {error}"))?
            .max(0) as u64,
        (None, Some(duration)) => now_unix_ms() + duration.as_millis() as u64,
        (None, None) if args.min_workers.is_some() => {
            anyhow::bail!("--min needs --until TIME or --for DURATION")
        }
        (None, None) if args.cap_workers.is_some() => {
            anyhow::bail!("--cap needs --until TIME or --for DURATION")
        }
        (None, None) => 0,
    };

    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let status = client
        .set_scale(pb::SetScaleRequest {
            app: app.to_owned(),
            release: release.to_owned(),
            group: args.group.clone(),
            processes: args.processes,
            unpin: args.unpin,
            min_workers: args.min_workers,
            max_workers: args.max_workers,
            cap_workers: args.cap_workers,
            until_unix_ms,
        })
        .await
        .map_err(explain)?
        .into_inner();

    if args.output.json {
        return print_answer(&status);
    }

    if let Some(floor) = args.min_workers {
        println!(
            "muru: {app} {} kept at {floor} workers or more until {}",
            args.group,
            format_time(until_unix_ms)
        );
        return Ok(());
    }

    if let Some(cap) = args.cap_workers {
        println!(
            "muru: {app} {} capped at {} until {}",
            args.group,
            count_workers(cap),
            format_time(until_unix_ms)
        );

        if !args.no_wait {
            wait_for_cap(&mut client, app, &args.group, cap).await?;
        }

        return Ok(());
    }

    match args.processes {
        Some(count) => println!(
            "muru: {}@{} {} pinned to {}",
            status.app,
            status.release,
            args.group,
            counted(count, "worker", "workers")
        ),
        None => println!(
            "muru: {}@{} {} unpinned; the autoscaler decides again",
            status.app, status.release, args.group
        ),
    }

    Ok(())
}

/// "1 worker" or "N workers".
fn count_workers(count: u32) -> String {
    if count == 1 {
        "1 worker".to_owned()
    } else {
        format!("{count} workers")
    }
}

/// Issue 712: wait until the serving release's group is at or under a
/// cap just set, polling its status, and say so. A pinned or advisory
/// group keeps its count, so that is said instead of waiting. The
/// autoscaler takes the cap at its next tick, so a status without it
/// is waited through for a few seconds before the cap counts as gone.
async fn wait_for_cap(
    client: &mut pb::admin_client::AdminClient<tonic::transport::Channel>,
    app: &str,
    group: &str,
    cap: u32,
) -> anyhow::Result<()> {
    let started = Instant::now();

    loop {
        let status = client
            .get_scale_status(pb::GetScaleStatusRequest {
                app: app.to_owned(),
                release: String::new(),
            })
            .await
            .map_err(explain)?
            .into_inner();
        let Some(found) = status.groups.iter().find(|found| found.group == group) else {
            anyhow::bail!("{app}@{} has no worker group named {group}", status.release);
        };

        if found.pinned {
            println!(
                "muru: {app}@{} {group} is pinned to {}, which the cap leaves as it is until --unpin",
                status.release,
                count_workers(found.desired)
            );
            return Ok(());
        }

        if found.advisory {
            println!(
                "muru: {app}@{} {group} has an advisory policy, which reports the cap without acting on it",
                status.release
            );
            return Ok(());
        }

        // A canary's share of the cap is what its release is held to.

        let held = if found.cap > 0 { found.cap } else { cap };

        if found.ready <= held && found.desired <= held {
            println!("muru: {app} {group} is at {}", count_workers(found.ready));
            return Ok(());
        }

        if found.cap == 0 && started.elapsed() > Duration::from_secs(5) {
            println!(
                "muru: {app} {group} is no longer capped, with {}",
                count_workers(found.ready)
            );
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// An application, or app@release; an empty release means the serving one.
fn split_target(text: &str) -> anyhow::Result<(&str, &str)> {
    match text.split_once('@') {
        Some((app, release)) if !app.is_empty() && !release.is_empty() => Ok((app, release)),
        Some(_) => anyhow::bail!("{text:?} is not a release: expected app@release"),
        None if text.is_empty() => anyhow::bail!("name an application or a release"),
        None => Ok((text, "")),
    }
}

/// The autoscaler's state, a row per group with its last decision below.
fn print_scale_status(status: &pb::ScaleStatus) {
    println!("{}@{}", status.app, status.release);

    if !status.traffic.is_empty() {
        println!("traffic: {}", status.traffic);
    }

    println!(
        "{:<12} {:<12} {:<10} {:<7} {:<13} {:<10} POLICY",
        "GROUP", "WORKERS", "SLOTS", "IDLE", "QUEUE-P95", "REAPS"
    );

    for group in &status.groups {
        let workers = if group.pinned {
            format!("{} pinned", group.desired)
        } else if group.desired != group.ready {
            format!("{} -> {}", group.ready, group.desired)
        } else {
            group.ready.to_string()
        };

        let reaps = if group.mode == "static" {
            "-".to_owned()
        } else if group.suspended {
            format!("{}/{} halt", group.reaps_last_hour, group.churn_budget)
        } else {
            format!("{}/{}", group.reaps_last_hour, group.churn_budget)
        };

        let p95 = match group.queue_wait_p95_ms {
            Some(ms) => muru_controller::scaling::describe_p95(
                ms,
                muru_controller::scaling::QUEUE_LARGEST_MS,
            ),
            None => "-".to_owned(),
        };

        println!(
            "{:<12} {:<12} {:<10} {:<7} {:<13} {:<10} {}",
            group.group,
            workers,
            format!("{:.0}/{:.0}", group.slots_busy, group.slots_total),
            format!("{:.2}", group.idle_workers),
            p95,
            reaps,
            group.policy
        );

        if !group.capacity_from.is_empty() {
            println!(
                "  scheduled: {}-{} workers{} from {}",
                group.floor,
                group.ceiling,
                if group.scale_in_held {
                    ", no scale-in"
                } else {
                    ""
                },
                group.capacity_from.join(", ")
            );
        }

        // Issue 712: a cap in force, and what sets it.

        if group.cap > 0 {
            println!(
                "  capped: at most {} from {}",
                count_workers(group.cap),
                group.cap_from.join(", ")
            );
        }

        // Section 7.3: the workers' own signals, and what they say.

        let p95 = |value: Option<f64>| {
            value.map_or("-".to_owned(), |ms| {
                muru_controller::scaling::describe_p95(
                    ms,
                    muru_controller::scaling::SIGNAL_LARGEST_MS,
                )
            })
        };

        // Section 6.14: the wait for a thread of requests handed from
        // ASGI to WSGI, for a group that handed any over.

        let handoff = group
            .handoff_queue_wait_p95_ms
            .map(|wait| format!(", handoff wait p95 {}", p95(Some(wait))))
            .unwrap_or_default();
        println!(
            "  workers: cpu {}, gc {}, gil wait p95 {}, loop lag p95 {}{handoff}",
            group
                .worker_cpu
                .map_or("-".to_owned(), muru_controller::scaling::describe_cpu),
            group
                .gc_share
                .map_or("-".to_owned(), |share| format!("{:.0}%", share * 100.0)),
            p95(group.gil_wait_p95_ms),
            p95(group.loop_lag_p95_ms)
        );

        if !group.diagnosis.is_empty() {
            if group.advice.is_empty() {
                println!("  diagnosis: {}", group.diagnosis);
            } else {
                println!(
                    "  diagnosis: {}; for the next release, {}",
                    group.diagnosis, group.advice
                );
            }
        }

        if !group.last_decision.is_empty() {
            println!(
                "  last: {} ({} ago)",
                group.last_decision,
                muru_config::format_duration(Duration::from_secs(
                    group.last_decision_age_ms / 1000
                ))
            );
        }
    }
}

/// Run a rollout subcommand.
pub async fn rollout(command: RolloutCommand) -> anyhow::Result<()> {
    match command {
        RolloutCommand::Canary {
            release,
            steps,
            hold,
            max_error_rate,
            max_p95,
            max_p99,
            max_worker_faults,
            max_queue_wait,
            probes,
            min_requests,
            approve_each_step,
            capacity,
            wait,
            override_freeze,
            socket,
            output,
        } => {
            let (app, id) = split_release(&release)?;
            let mut client = client::connect(&socket.endpoint()?).await?;
            let rollout = client
                .start_rollout(pb::StartRolloutRequest {
                    app: app.to_owned(),
                    release: id.to_owned(),
                    steps: steps
                        .iter()
                        .map(|percent| pb::RolloutStep {
                            percent: *percent,
                            hold_ms: hold.as_millis() as u64,
                        })
                        .collect(),
                    max_error_rate: max_error_rate / 100.0,
                    max_p95_ms: max_p95.map_or(0, |p95| p95.as_millis() as u64),
                    max_p99_ms: max_p99.map_or(0, |p99| p99.as_millis() as u64),
                    max_worker_faults,
                    max_queue_wait_p95_ms: max_queue_wait.map_or(0, |wait| wait.as_millis() as u64),
                    probes,
                    min_requests,
                    approve_each_step,
                    override_freeze,
                    capacity,
                })
                .await
                .map_err(explain)?
                .into_inner();

            // With --json, the rollout as it finished when waited for,
            // else as it started.

            if output.json {
                return match wait {
                    true => print_answer(&wait_rollout(&mut client, app, &rollout.id).await?),
                    false => print_answer(&rollout),
                };
            }

            println!(
                "muru: rollout {} of {}@{} started against {}: {}",
                rollout.id, rollout.app, rollout.candidate, rollout.baseline, rollout.message
            );

            if wait {
                let finished = wait_rollout(&mut client, app, &rollout.id).await?;
                println!(
                    "muru: rollout {} {}: {}",
                    finished.id, finished.state, finished.message
                );
            }

            Ok(())
        }
        RolloutCommand::Bluegreen {
            release,
            override_freeze,
            socket,
            output,
        } => {
            let (app, id) = split_release(&release)?;
            let mut client = client::connect(&socket.endpoint()?).await?;
            let release = client
                .promote_release(pb::PromoteReleaseRequest {
                    app: app.to_owned(),
                    release: id.to_owned(),
                    override_freeze,
                })
                .await
                .map_err(explain)?
                .into_inner();

            if output.json {
                return print_answer(&release);
            }

            println!(
                "muru: release {}@{} is {} and serving",
                release.app, release.id, release.state
            );
            Ok(())
        }
        RolloutCommand::Status {
            app,
            wait,
            socket,
            output,
        } => {
            let mut client = client::connect(&socket.endpoint()?).await?;

            if wait {
                let Some(app) = app else {
                    anyhow::bail!("--wait needs an application");
                };
                let finished = wait_rollout(&mut client, &app, "").await?;

                if output.json {
                    return print_answer(&finished);
                }

                println!(
                    "muru: rollout {} {}: {}",
                    finished.id, finished.state, finished.message
                );
                return Ok(());
            }

            let answer = client
                .list_rollouts(pb::ListRolloutsRequest {
                    app: app.unwrap_or_default(),
                })
                .await
                .map_err(explain)?
                .into_inner();

            if output.json {
                return print_answer(&answer);
            }

            let rollouts = answer.rollouts;

            if rollouts.is_empty() {
                println!("no rollouts");
                return Ok(());
            }

            println!(
                "{:<16} {:<8} {:<12} {:<12} {:<18} {:<8} MESSAGE",
                "APP", "ROLLOUT", "CANDIDATE", "BASELINE", "STATE", "TRAFFIC"
            );

            for rollout in rollouts {
                println!(
                    "{:<16} {:<8} {:<12} {:<12} {:<18} {:<8} {}",
                    rollout.app,
                    rollout.id,
                    rollout.candidate,
                    rollout.baseline,
                    rollout.state,
                    format!("{}%", rollout.percent),
                    rollout.message
                );
            }

            Ok(())
        }
        RolloutCommand::Pause {
            app,
            socket,
            output,
        } => {
            let mut client = client::connect(&socket.endpoint()?).await?;
            let rollout = client
                .pause_rollout(pb::RolloutControlRequest {
                    app,
                    override_freeze: false,
                })
                .await
                .map_err(explain)?
                .into_inner();

            if output.json {
                return print_answer(&rollout);
            }

            println!(
                "muru: rollout {} {}: {}",
                rollout.id, rollout.state, rollout.message
            );
            Ok(())
        }
        RolloutCommand::Resume {
            app,
            socket,
            output,
        } => {
            let mut client = client::connect(&socket.endpoint()?).await?;
            let rollout = client
                .resume_rollout(pb::RolloutControlRequest {
                    app,
                    override_freeze: false,
                })
                .await
                .map_err(explain)?
                .into_inner();

            if output.json {
                return print_answer(&rollout);
            }

            println!(
                "muru: rollout {} {}: {}",
                rollout.id, rollout.state, rollout.message
            );
            Ok(())
        }
        RolloutCommand::Promote {
            app,
            override_freeze,
            socket,
            output,
        } => {
            let mut client = client::connect(&socket.endpoint()?).await?;
            let rollout = client
                .promote_rollout(pb::RolloutControlRequest {
                    app,
                    override_freeze,
                })
                .await
                .map_err(explain)?
                .into_inner();

            if output.json {
                return print_answer(&rollout);
            }

            println!(
                "muru: rollout {} {}: {}",
                rollout.id, rollout.state, rollout.message
            );
            Ok(())
        }
        RolloutCommand::Abort {
            app,
            socket,
            output,
        } => {
            let mut client = client::connect(&socket.endpoint()?).await?;
            let rollout = client
                .abort_rollout(pb::RolloutControlRequest {
                    app,
                    override_freeze: false,
                })
                .await
                .map_err(explain)?
                .into_inner();

            if output.json {
                return print_answer(&rollout);
            }

            println!(
                "muru: rollout {} {}: {}",
                rollout.id, rollout.state, rollout.message
            );
            Ok(())
        }
    }
}

/// Poll a rollout until it is promoted or rolled back, printing each
/// change of step or state on the way.
async fn wait_rollout(
    client: &mut pb::admin_client::AdminClient<tonic::transport::Channel>,
    app: &str,
    id: &str,
) -> anyhow::Result<pb::Rollout> {
    let mut last = String::new();

    loop {
        let rollout = client
            .get_rollout(pb::GetRolloutRequest {
                app: app.to_owned(),
                rollout: id.to_owned(),
            })
            .await
            .map_err(explain)?
            .into_inner();

        if matches!(rollout.state.as_str(), "promoted" | "rolled_back") {
            return Ok(rollout);
        }

        let line = format!("{} {}", rollout.state, rollout.message);

        if line != last {
            println!("muru: rollout {} {line}", rollout.id);
            last = line;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The application subcommands.
#[derive(Subcommand)]
pub enum AppCommand {
    /// List the applications.
    List {
        /// Only what this deployment owns.
        #[arg(long, value_name = "NAME")]
        deployment: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Remove an application: drain whatever runs and remove every
    /// release with its environment, bytecode, static directory and logs.
    /// Its data directory is kept unless --data. Refused while a route
    /// sends it traffic. Asks first; without a terminal it needs --yes.
    Remove(Box<AppRemoveArgs>),

    /// Set release settings on the application, which every release
    /// deployed from now takes over the source's [tool.muru], and under a
    /// deploy's own flags: variables, secrets, worker counts, recycling,
    /// the host's timeouts and the memory policy. No running release
    /// changes. The application is created when it does not exist.
    Set(Box<AppSetArgs>),

    /// Remove release settings from the application, by key: processes,
    /// threads, sync_threads, concurrency, max_requests, max_lifetime,
    /// max_surge, max_unavailable, timeouts.KEY, memory.KEY, env.NAME,
    /// or env for every variable.
    Unset(Box<AppUnsetArgs>),

    /// Show the release settings the application gives its releases.
    Show {
        /// The application.
        app: String,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// What `muructl app set` takes: the deploy flags an application's
/// settings hold.
#[derive(Args, Clone)]
pub struct AppSetArgs {
    /// The application.
    app: String,

    /// Worker processes.
    #[arg(long)]
    processes: Option<u32>,

    /// Application threads per worker.
    #[arg(long)]
    threads: Option<u32>,

    /// Threads for synchronous code called from an ASGI application.
    #[arg(long, value_name = "N")]
    sync_threads: Option<u32>,

    /// Requests an ASGI worker's event loop takes at once.
    #[arg(long, value_name = "N")]
    concurrency: Option<u32>,

    /// Replace a worker after this many requests, with jitter.
    #[arg(long, value_name = "N")]
    max_requests: Option<u64>,

    /// Replace a worker after this long, for example 1h, with jitter.
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    max_lifetime: Option<Duration>,

    /// Workers a group may run over its count while workers are swapped
    /// (default 1): a replacement starts before the old worker stops
    /// while the group is within it, and with 0 the old worker stops
    /// first.
    #[arg(long, value_name = "N")]
    max_surge: Option<u32>,

    /// Workers a group may run under its count while workers are
    /// swapped (default 0).
    #[arg(long, value_name = "N")]
    max_unavailable: Option<u32>,

    /// A host timeout as NAME=DURATION, as deploy --timeout takes it:
    /// soft, hard, hard_drain_grace, shutdown, websocket_drain_grace,
    /// exit_grace, loop_stall or gil_stall. Repeatable.
    #[arg(long = "timeout", value_name = "NAME=DURATION", value_parser = parse_host_timeout)]
    timeouts: Vec<(String, Duration)>,

    /// A memory setting as KEY=VALUE, as deploy --memory takes it:
    /// measure, soft_limit, soft_for, reclaim_first, hard_limit or
    /// check_interval. Repeatable.
    #[arg(long = "memory", value_name = "KEY=VALUE", value_parser = crate::parse_memory_setting)]
    memory: Vec<(String, String)>,

    /// An environment variable as NAME=VALUE. Repeatable.
    #[arg(long = "env", value_name = "NAME=VALUE", value_parser = parse_env_setting)]
    env: Vec<(String, String)>,

    /// A secret as NAME=STORE:KEY, delivered as the variable, or
    /// NAME=STORE:KEY:file for a file named by NAME_FILE; the reference is
    /// kept, never the value. Repeatable.
    #[arg(long = "secret", value_name = "NAME=STORE:KEY[:file]", value_parser = parse_secret_setting)]
    secrets: Vec<(String, String, String, bool)>,

    /// Remove every setting the application has first, so it holds only
    /// those given here.
    #[arg(long)]
    replace: bool,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// What `muructl app unset` takes.
#[derive(Args, Clone)]
pub struct AppUnsetArgs {
    /// The application.
    app: String,

    /// The settings to remove, by key.
    #[arg(value_name = "KEY", required_unless_present = "all")]
    keys: Vec<String>,

    /// Remove every setting.
    #[arg(long, conflicts_with = "keys")]
    all: bool,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    output: JsonArgs,
}

impl AppSetArgs {
    /// The settings given, as the admin message carries them.
    fn settings(&self) -> pb::AppSettings {
        let timeout = |name: &str| timeout_ms(&self.timeouts, name);

        pb::AppSettings {
            env: self
                .env
                .iter()
                .map(|(name, value)| pb::EnvEntry {
                    name: name.clone(),
                    value: value.clone(),
                    ..Default::default()
                })
                .chain(
                    self.secrets
                        .iter()
                        .map(|(name, store, key, as_file)| pb::EnvEntry {
                            name: name.clone(),
                            secret_store: store.clone(),
                            secret_key: key.clone(),
                            as_file: *as_file,
                            ..Default::default()
                        }),
                )
                .collect(),
            processes: self.processes,
            threads: self.threads,
            sync_threads: self.sync_threads,
            concurrency: self.concurrency,
            max_requests: self.max_requests,
            max_lifetime_ms: self
                .max_lifetime
                .map(|lifetime| lifetime.as_millis() as u64),
            max_surge: self.max_surge,
            max_unavailable: self.max_unavailable,
            soft_timeout_ms: timeout("soft"),
            hard_timeout_ms: timeout("hard"),
            hard_drain_grace_ms: timeout("hard_drain_grace"),
            shutdown_timeout_ms: timeout("shutdown"),
            websocket_drain_grace_ms: timeout("websocket_drain_grace"),
            exit_grace_ms: timeout("exit_grace"),
            loop_stall_timeout_ms: timeout("loop_stall"),
            gil_stall_timeout_ms: timeout("gil_stall"),
            memory_measure: memory_setting(&self.memory, "measure").unwrap_or_default(),
            memory_soft_limit: memory_setting(&self.memory, "soft_limit")
                .and_then(|text| muru_config::parse_size(&text).ok()),
            memory_soft_for_ms: memory_setting(&self.memory, "soft_for")
                .and_then(|text| muru_config::parse_duration(&text).ok())
                .map(|duration| duration.as_millis() as u64),
            memory_reclaim_first: memory_setting(&self.memory, "reclaim_first")
                .map(|text| text == "true"),
            memory_hard_limit: memory_setting(&self.memory, "hard_limit")
                .and_then(|text| muru_config::parse_size(&text).ok()),
            memory_check_interval_ms: memory_setting(&self.memory, "check_interval")
                .and_then(|text| muru_config::parse_duration(&text).ok())
                .map(|duration| duration.as_millis() as u64),
            notice: String::new(),
        }
    }
}

/// Set an application's release settings.
pub async fn app_set(args: AppSetArgs) -> anyhow::Result<()> {
    let settings = args.settings();

    anyhow::ensure!(
        args.replace || settings != pb::AppSettings::default(),
        "give a setting to set, such as --env NAME=VALUE or --processes N; \
         `muructl app unset` removes them"
    );

    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let answer = client
        .set_app_settings(pb::SetAppSettingsRequest {
            app: args.app.clone(),
            set: Some(settings),
            unset: Vec::new(),
            clear: args.replace,
        })
        .await
        .map_err(explain)?
        .into_inner();

    report_app_settings(&args.app, &answer, args.output.json)
}

/// Remove some of an application's release settings, or all of them.
pub async fn app_unset(args: AppUnsetArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let answer = client
        .set_app_settings(pb::SetAppSettingsRequest {
            app: args.app.clone(),
            set: None,
            unset: args.keys.clone(),
            clear: args.all,
        })
        .await
        .map_err(explain)?
        .into_inner();

    report_app_settings(&args.app, &answer, args.output.json)
}

/// Show an application's release settings.
pub async fn app_show(app: String, socket: SocketArgs, json: bool) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let answer = client
        .get_app(pb::GetAppRequest { name: app.clone() })
        .await
        .map_err(explain)?
        .into_inner();
    let settings = answer.settings.unwrap_or_default();

    if json {
        return print_answer(&settings);
    }

    print_app_settings(&app, &settings)
}

/// What `app set` and `app unset` say: that the change waits for the next
/// deploy, then the settings as they now are.
fn report_app_settings(app: &str, settings: &pb::AppSettings, json: bool) -> anyhow::Result<()> {
    if json {
        return print_answer(settings);
    }

    println!(
        "muru: {app}'s settings are changed for the releases deployed from now; \
         no running release changes"
    );

    if !settings.notice.is_empty() {
        eprintln!("muru: warning: {}", settings.notice);
    }

    print_app_settings(app, settings)
}

/// An application's settings, a line a key, a secret by its reference.
fn print_app_settings(app: &str, settings: &pb::AppSettings) -> anyhow::Result<()> {
    let lines = muru_controller::app_settings::AppSettings::from_message(settings)
        .map_err(|error| anyhow::anyhow!(error))?
        .lines();

    if lines.is_empty() {
        println!(
            "{app} has no settings of its own; its releases take the flags they are deployed \
             with, the source's [tool.muru] and the defaults"
        );
    }

    for (key, value) in lines {
        println!("{key} = {value}");
    }

    Ok(())
}

/// What `muructl app remove` takes.
#[derive(Args, Clone)]
pub struct AppRemoveArgs {
    /// The application.
    app: String,

    /// Remove its data directory too, with the databases and files every
    /// release shared; a part kept elsewhere, by configuration or through
    /// a symbolic link, stays.
    #[arg(long)]
    data: bool,

    /// Only say what would go.
    #[arg(long)]
    dry_run: bool,

    /// Do it without asking.
    #[arg(long)]
    yes: bool,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// List applications.
/// Keep what one deployment owns, when one is named.
fn of_deployment<T>(items: &mut Vec<T>, deployment: Option<&str>, owner: impl Fn(&T) -> &str) {
    if let Some(deployment) = deployment {
        items.retain(|item| owner(item) == deployment);
    }
}

/// Whether a list has a DEPLOYMENT column: when something in it belongs
/// to a named deployment. A node that names none sees the lists as they
/// are without deployments.
fn names_deployments<'a>(mut owners: impl Iterator<Item = &'a str>) -> bool {
    owners.any(|owner| !owner.is_empty() && owner != muru_controller::deployments::DEFAULT)
}

/// The DEPLOYMENT column's cell, or nothing when the list has none.
fn deployment_cell(shown: bool, deployment: &str) -> String {
    if shown {
        format!("{:<16} ", or_dash(deployment))
    } else {
        String::new()
    }
}

pub async fn app_list(
    socket: SocketArgs,
    json: bool,
    deployment: Option<String>,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let mut answer = client.list_apps(pb::ListAppsRequest {}).await?.into_inner();
    of_deployment(&mut answer.apps, deployment.as_deref(), |app| {
        &app.deployment
    });

    if json {
        return print_answer(&answer);
    }

    let apps = answer.apps;

    if apps.is_empty() {
        println!("no applications");
        return Ok(());
    }

    let owned = names_deployments(apps.iter().map(|app| app.deployment.as_str()));
    println!(
        "{:<20} {}{:<12} {:<12} USER",
        "APP",
        deployment_cell(owned, "DEPLOYMENT"),
        "CURRENT",
        "PREVIOUS"
    );

    for app in apps {
        let user = if app.user.is_empty() {
            "-".to_owned()
        } else if app.group.is_empty() {
            app.user.clone()
        } else {
            format!("{}:{}", app.user, app.group)
        };
        println!(
            "{:<20} {}{:<12} {:<12} {user}{}{}{}",
            app.name,
            deployment_cell(owned, &app.deployment),
            or_dash(&app.current_release),
            or_dash(&app.previous_release),
            if app.umask.is_empty() {
                String::new()
            } else {
                format!(" umask {}", app.umask)
            },
            if app.priority == 0 {
                String::new()
            } else {
                format!(" [priority {}]", app.priority)
            },
            maintenance_mark(&app)
        );
    }

    Ok(())
}

/// How `app list` marks an application's maintenance, naming the overlay
/// that set it, or that turned it off, when one did (issue 757).
fn maintenance_mark(app: &pb::App) -> String {
    match (app.maintenance, app.maintenance_overlay.as_str()) {
        (true, "") => " [maintenance]".to_owned(),
        (true, overlay) => format!(" [maintenance by overlay {overlay}]"),
        (false, "") => String::new(),
        (false, overlay) => format!(" [maintenance off by overlay {overlay}]"),
    }
}

/// Remove an application and all its releases.
pub async fn app_remove(args: AppRemoveArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let request = pb::RemoveAppRequest {
        app: args.app.clone(),
        dry_run: true,
        data: args.data,
    };

    let plan = client
        .remove_app(request.clone())
        .await
        .map_err(explain)?
        .into_inner();

    if args.dry_run {
        if !args.output.json && plan.removed.is_empty() {
            println!("muru: {} has no releases to remove", args.app);
            return Ok(());
        }

        show_removal_plan(&plan, true, args.output.json)?;
        return Ok(());
    }

    for release in &plan.removed {
        eprintln!("  {}", removal_line(release));
    }

    let question = format!(
        "Remove application {} and its {} release{}, freeing {}{}?",
        args.app,
        plan.removed.len(),
        if plan.removed.len() == 1 { "" } else { "s" },
        human_size(plan.freed),
        if plan.data_removed {
            format!(", and its data directory {}", plan.data_dir)
        } else {
            String::new()
        }
    );

    if !confirm(&question, args.yes)? {
        eprintln!("muru: nothing was changed");
        return Ok(());
    }

    let answer = client
        .remove_app(pb::RemoveAppRequest {
            dry_run: false,
            ..request
        })
        .await
        .map_err(explain)?
        .into_inner();

    let finished = finish_removal(&answer, args.output.json);

    // Issue 849: where the data directory is, kept or removed, and the
    // parts a removal leaves.

    if !args.output.json && !answer.data_dir.is_empty() {
        if answer.data_removed {
            println!("muru: removed the data directory {}", answer.data_dir);
        } else {
            println!(
                "muru: the data directory {} is kept; muructl app remove {} --data removes it too",
                answer.data_dir, args.app
            );
        }

        for part in &answer.data_kept {
            println!("muru: the {part} is outside the data directory and is kept");
        }
    }

    finished
}

/// The `muructl data` subcommands (issue 849).
#[derive(Subcommand)]
pub enum DataCommand {
    /// Where an application's data directory and its media and uploads
    /// parts are, where each leads, its owner, mode and file system,
    /// which is a file hint root, and whether uploads share the upload
    /// spool's file system.
    Show(Box<DataArgs>),

    /// Warn of a database file in the media part, a part on a network
    /// file system and a part not owned by the application's user;
    /// exits 1 when there is any.
    Check(Box<DataArgs>),
}

/// What `muructl data show` and `check` take.
#[derive(Args)]
pub struct DataArgs {
    /// The application; none for a development run's or a serve's own.
    app: Option<String>,

    /// Add what each part holds, which walks it.
    #[arg(long)]
    sizes: bool,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// Ask for an application's data directory.
async fn app_data(
    endpoint: &client::Endpoint,
    app: &str,
    sizes: bool,
) -> anyhow::Result<pb::AppData> {
    let mut client = client::connect(endpoint).await?;
    Ok(client
        .get_app_data(pb::GetAppDataRequest {
            app: app.to_owned(),
            sizes,
        })
        .await
        .map_err(explain)?
        .into_inner())
}

/// `muructl data show`: each part in full.
pub async fn data_show(args: DataArgs) -> anyhow::Result<()> {
    let data = app_data(
        &args.socket.endpoint()?,
        args.app.as_deref().unwrap_or_default(),
        args.sizes,
    )
    .await?;

    if args.output.json {
        return print_answer(&data);
    }

    print!("{}", data_lines(&data, true));
    Ok(())
}

/// `muructl data check`: the warnings, exiting 1 when there is any.
pub async fn data_check(args: DataArgs) -> anyhow::Result<i32> {
    let data = app_data(
        &args.socket.endpoint()?,
        args.app.as_deref().unwrap_or_default(),
        false,
    )
    .await?;

    if args.output.json {
        print_answer(&data.warnings)?;
    } else if data.warnings.is_empty() {
        if data.none.is_empty() {
            println!(
                "muru: the data directory of {} has nothing to warn of",
                data.app
            );
        } else {
            println!("muru: {}", data.none);
        }
    } else {
        for warning in &data.warnings {
            println!("warning: {warning}");
        }
    }

    Ok(i32::from(!data.warnings.is_empty()))
}

/// An application's data directory as lines: one per part, and in full
/// its owner, mode and file system, and whether uploads share the
/// spool's file system.
pub fn data_lines(data: &pb::AppData, full: bool) -> String {
    use muru_config::format_size_approx as size;

    let mut text = String::new();

    if data.parts.is_empty() {
        text.push_str(&format!("data     none: {}\n", data.none));
        return text;
    }

    for part in &data.parts {
        let mut line = format!("{:<8} {}", part.name, part.path);

        if !part.resolved.is_empty() {
            line.push_str(&format!(" -> {}", part.resolved));
        }

        if !part.exists {
            line.push_str(", not there yet");
        } else {
            let owner = if part.owner.is_empty() {
                format!("uid {}", part.owner_uid)
            } else {
                part.owner.clone()
            };
            line.push_str(&format!(", {owner}"));

            if full {
                line.push_str(&format!(" {:04o}", part.mode));

                if !part.filesystem.is_empty() {
                    line.push_str(&format!(", {}", part.filesystem));

                    if part.network {
                        line.push_str(" (network)");
                    }
                }
            }
        }

        if part.root {
            line.push_str(", a file hint root");
        }

        if let Some(bytes) = part.bytes {
            line.push_str(&format!(", {}", size(bytes)));
        }

        text.push_str(&line);
        text.push('\n');
    }

    if full && !data.spool.is_empty() {
        text.push_str(&format!(
            "spool    {}, {}\n",
            data.spool,
            if data.spool_shared {
                "on the uploads part's file system, so upload.save() links"
            } else {
                "on another file system from the uploads part, so upload.save() copies"
            }
        ));
    }

    if full {
        for warning in &data.warnings {
            text.push_str(&format!("warning: {warning}\n"));
        }
    }

    text
}

/// The `muructl condition` subcommands (section 9.10).
#[derive(Subcommand)]
pub enum ConditionCommand {
    /// Every condition, its state and since when.
    List {
        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// One condition: its expressions, its state, and each term's figure
    /// now, or why it has none.
    Show {
        /// The condition.
        name: String,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// Run a `muructl condition` subcommand.
pub async fn condition(command: ConditionCommand) -> anyhow::Result<()> {
    let (name, socket, json) = match &command {
        ConditionCommand::List { socket, output } => (String::new(), socket, output.json),
        ConditionCommand::Show {
            name,
            socket,
            output,
        } => (name.clone(), socket, output.json),
    };

    let mut client = client::connect(&socket.endpoint()?).await?;
    let answer = client
        .list_conditions(pb::ListConditionsRequest { name: name.clone() })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        return match answer.conditions.first().filter(|_| !name.is_empty()) {
            Some(condition) => print_answer(condition),
            None => print_answer(&answer),
        };
    }

    if name.is_empty() {
        if answer.conditions.is_empty() {
            println!("no conditions are declared");
            return Ok(());
        }

        let width = answer
            .conditions
            .iter()
            .map(|condition| condition.name.len())
            .max()
            .unwrap_or(0)
            .max("CONDITION".len());
        println!(
            "{:<width$} {:<9} {:<21} WHEN",
            "CONDITION", "STATE", "SINCE"
        );

        for condition in &answer.conditions {
            // Issue 727: one waiting out `for` to fire is pending.

            let state = if condition.pending_since_unix_ms > 0 && condition.state != "firing" {
                "pending"
            } else {
                condition.state.as_str()
            };

            println!(
                "{:<width$} {:<9} {:<21} {}",
                condition.name,
                state,
                condition_since(condition),
                condition.when
            );
        }

        return Ok(());
    }

    for condition in &answer.conditions {
        println!("condition {} ({})", condition.name, condition.source);
        println!("  state       {}", condition_state(condition));
        println!("  when        {}", condition.when);

        if !condition.r#for.is_empty() {
            println!("  for         {}", condition.r#for);
        }

        if !condition.clear_when.is_empty() {
            println!("  clear_when  {}", condition.clear_when);
        }

        if !condition.clear_for.is_empty() {
            println!("  clear_for   {}", condition.clear_for);
        }

        println!("  min         {} requests", condition.min_requests);

        for term in &condition.terms {
            let figure = if term.known {
                format!("{}", round_figure(term.value))
            } else {
                format!("unknown: {}", term.unknown)
            };
            println!("  term        {} is {figure}", term.term);
        }
    }

    Ok(())
}

/// When a condition took its state, or a dash before its first figure.
fn condition_since(condition: &pb::ConditionStatus) -> String {
    if condition.since_unix_ms == 0 {
        "-".to_owned()
    } else {
        format_time(condition.since_unix_ms)
    }
}

/// A condition's state in words: unknown says why and what it holds,
/// and one waiting out `for` or `clear_for` says how long it has waited
/// (issue 727).
fn condition_state(condition: &pb::ConditionStatus) -> String {
    let wait = condition_wait(condition, now_unix_ms());

    if condition.state != "unknown" {
        let state = format!("{} since {}", condition.state, condition_since(condition));

        return match wait {
            Some(wait) if condition.state == "firing" => format!("{state}; {wait}"),
            Some(wait) => format!("pending: {wait}; {state}"),
            None => state,
        };
    }

    // Before its first change, a condition whose `when` holds from its
    // first figure has no state to show, only its wait.

    if condition.unknown.is_empty() {
        return match wait {
            Some(wait) => format!("pending: {wait}"),
            None => "unknown: no state yet".to_owned(),
        };
    }

    if condition.held == "unknown" {
        format!("unknown: {}", condition.unknown)
    } else {
        format!(
            "unknown, holding {} since {}: {}",
            condition.held,
            condition_since(condition),
            condition.unknown
        )
    }
}

/// How long the expression a condition waits on has held, against the
/// wait it needs, as `when has held for 4s of 10s`; none when it is not
/// waiting.
fn condition_wait(condition: &pb::ConditionStatus, now_unix_ms: u64) -> Option<String> {
    if condition.pending_since_unix_ms == 0 {
        return None;
    }

    let held =
        Duration::from_secs(now_unix_ms.saturating_sub(condition.pending_since_unix_ms) / 1000);
    let firing = condition.state == "firing" || condition.held == "firing";

    let (what, needed) = match (firing, condition.clear_when.is_empty()) {
        (false, _) => ("when has held", &condition.r#for),
        (true, false) => ("clear_when has held", &condition.clear_for),
        (true, true) => ("when has been false", &condition.clear_for),
    };

    let needed = if needed.is_empty() { "0s" } else { needed };

    let held = if held.is_zero() {
        "0s".to_owned()
    } else {
        muru_config::format_duration(held)
    };

    Some(format!("{what} for {held} of {needed}"))
}

/// A figure to four significant digits.
fn round_figure(value: f64) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }

    let scale = 10f64.powi(3 - value.abs().log10().floor() as i32);
    (value * scale).round() / scale
}

/// The `muructl overlay` subcommands (section 9.10).
#[derive(Subcommand)]
pub enum OverlayCommand {
    /// Every overlay, whether it is active and what it changes.
    List {
        /// Only what this deployment owns.
        #[arg(long, value_name = "NAME")]
        deployment: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Apply an overlay now, until ended, or for a time.
    Activate {
        /// The overlay.
        name: String,

        /// End it on its own after this long, for example 2h.
        #[arg(long = "for", value_name = "DURATION", value_parser = muru_config::parse_duration, conflicts_with = "until")]
        duration: Option<Duration>,

        /// End it on its own at this time, such as 2026-11-02T09:00 or an
        /// RFC 3339 time.
        #[arg(long, value_name = "TIME")]
        until: Option<String>,

        /// The time zone --until is in; the local zone without it.
        #[arg(long, value_name = "ZONE", requires = "until")]
        timezone: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Stop applying an overlay; one its schedule activated stays ended
    /// until the schedule's window is over.
    End {
        /// The overlay.
        name: String,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// What activating an overlay now would change.
    Preview {
        /// The overlay.
        name: String,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Fold an active overlay's changes into the base configuration and
    /// end it for good.
    Promote {
        /// The overlay.
        name: String,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// List, activate, end, preview or promote overlays.
pub async fn overlay(command: OverlayCommand) -> anyhow::Result<()> {
    let (name, action, until, socket, json) = match command {
        OverlayCommand::List {
            deployment,
            socket,
            output,
        } => {
            return list_overlays(&socket, output.json, deployment).await;
        }
        OverlayCommand::Preview {
            name,
            socket,
            output,
        } => {
            let mut client = client::connect(&socket.endpoint()?).await?;
            let preview = client
                .preview_overlay(pb::PreviewOverlayRequest { name: name.clone() })
                .await
                .map_err(explain)?
                .into_inner();

            if output.json {
                return print_answer(&preview);
            }

            if !preview.condition.is_empty() {
                println!(
                    "condition {} is {}",
                    preview.condition,
                    if preview.condition_state.is_empty() {
                        "not declared"
                    } else {
                        preview.condition_state.as_str()
                    }
                );

                for term in &preview.condition_terms {
                    println!("  {term}");
                }
            }

            if preview.changes.is_empty() {
                println!("muru: activating overlay {name} would change nothing now");
            }

            for change in preview.changes {
                println!("{change}");
            }

            return Ok(());
        }
        OverlayCommand::Activate {
            name,
            duration,
            until,
            timezone,
            socket,
            output,
        } => {
            let until = match (duration, until) {
                (Some(duration), _) => Some(now_unix_ms() + duration.as_millis() as u64),
                (None, Some(text)) => Some(
                    muru_config::parse_moment(&text, timezone.as_deref())
                        .map_err(anyhow::Error::msg)?
                        .max(0) as u64,
                ),
                (None, None) => None,
            };

            (name, "activate", until, socket, output.json)
        }
        OverlayCommand::End {
            name,
            socket,
            output,
        } => (name, "end", None, socket, output.json),
        OverlayCommand::Promote {
            name,
            socket,
            output,
        } => (name, "promote", None, socket, output.json),
    };

    let mut client = client::connect(&socket.endpoint()?).await?;
    let response = client
        .control_overlay(pb::ControlOverlayRequest {
            name: name.clone(),
            action: action.to_owned(),
            until_unix_ms: until.unwrap_or(0),
        })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        return print_answer(&response);
    }

    println!(
        "muru: overlay {name} {}{}",
        response.message,
        until
            .map(|until| format!(", until {}", format_time(until)))
            .unwrap_or_default()
    );

    // Issue 759: the applied files still say what the overlay changed,
    // so the next apply would undo a promotion they are not brought into
    // line with.

    if !response.files.is_empty() {
        println!(
            "muru: the next apply undoes this unless the applied files say the same: put this in \
             them, and remove overlay {name}\n"
        );
        println!("{}", response.files.join("\n\n"));
    }

    Ok(())
}

async fn list_overlays(
    socket: &SocketArgs,
    json: bool,
    deployment: Option<String>,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let mut answer = client
        .list_overlays(pb::ListOverlaysRequest {})
        .await
        .map_err(explain)?
        .into_inner();
    of_deployment(&mut answer.overlays, deployment.as_deref(), |overlay| {
        &overlay.deployment
    });

    if json {
        return print_answer(&answer);
    }

    let overlays = answer.overlays;

    if overlays.is_empty() {
        println!("muru: no overlays are declared");
        return Ok(());
    }

    for line in overlay_table(&overlays) {
        println!("{line}");
    }

    Ok(())
}

/// The lines of `muructl overlay list`: each column as wide as its widest
/// value, and no narrower than it always was (issue 728).
fn overlay_table(overlays: &[pb::OverlayInfo]) -> Vec<String> {
    let rows: Vec<[String; 4]> = overlays.iter().map(overlay_row).collect();
    let width = |column: usize, least: usize| {
        rows.iter()
            .map(|row| row[column].len())
            .max()
            .unwrap_or(0)
            .max(least)
    };
    let (name, state, trigger) = (width(0, 24), width(1, 10), width(2, 34));
    let owned = names_deployments(overlays.iter().map(|overlay| overlay.deployment.as_str()));
    let mut lines = vec![format!(
        "{:<name$} {}{:<state$} {:<trigger$} CHANGES",
        "OVERLAY",
        deployment_cell(owned, "DEPLOYMENT"),
        "STATE",
        "TRIGGER"
    )];

    for (row, overlay) in rows.iter().zip(overlays) {
        lines.push(format!(
            "{:<name$} {}{:<state$} {:<trigger$} {}",
            row[0],
            deployment_cell(owned, &overlay.deployment),
            row[1],
            row[2],
            row[3]
        ));
    }

    lines
}

/// An overlay's row in `muructl overlay list`: its name, its state, its
/// trigger with its condition's state, and what it changes.
fn overlay_row(overlay: &pb::OverlayInfo) -> [String; 4] {
    let mut state = if overlay.state.is_empty() {
        "idle".to_owned()
    } else if overlay.active {
        format!("active ({})", overlay.state)
    } else {
        overlay.state.clone()
    };

    // A hold with no end (a skip, say, in a window with no end) lasts
    // until a person acts, which the next window's start would belie
    // (issue 755).

    let held = !overlay.active && !overlay.state.is_empty();

    if overlay.until_unix_ms > 0 {
        state.push_str(&format!(" until {}", format_time(overlay.until_unix_ms)));
    } else if held && overlay.state == "skipped" {
        state.push_str(" (activate it by hand, or apply a change, to try again)");
    } else if !held && overlay.starts_unix_ms > 0 {
        state.push_str(&format!(" from {}", format_time(overlay.starts_unix_ms)));
    }

    let trigger = if overlay.condition.is_empty() {
        overlay.trigger.clone()
    } else {
        format!("{}, {}", overlay.trigger, overlay.condition_state)
    };

    [
        overlay.name.clone(),
        state,
        trigger,
        overlay.changes.join(", "),
    ]
}

/// The `muructl maintenance` subcommands (section 4.12).
#[derive(Subcommand)]
pub enum MaintenanceCommand {
    /// Serve the maintenance page at the edge for an application, or
    /// with --host for every route of a virtual host: 503 maintenance,
    /// except for what its exemptions let through.
    On {
        /// The application.
        #[arg(required_unless_present = "host", conflicts_with = "host")]
        app: Option<String>,

        /// The virtual host, instead of an application.
        #[arg(long, value_name = "HOST")]
        host: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Serve the application, or with --host the virtual host, as usual
    /// again.
    Off {
        /// The application.
        #[arg(required_unless_present = "host", conflicts_with = "host")]
        app: Option<String>,

        /// The virtual host, instead of an application.
        #[arg(long, value_name = "HOST")]
        host: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// Turn maintenance mode on or off.
pub async fn maintenance(command: MaintenanceCommand) -> anyhow::Result<()> {
    let (app, host, on, socket, json) = match command {
        MaintenanceCommand::On {
            app,
            host,
            socket,
            output,
        } => (app, host, true, socket, output.json),
        MaintenanceCommand::Off {
            app,
            host,
            socket,
            output,
        } => (app, host, false, socket, output.json),
    };

    let mut client = client::connect(&socket.endpoint()?).await?;
    let status = client
        .set_maintenance(pb::SetMaintenanceRequest {
            app: app.unwrap_or_default(),
            host: host.unwrap_or_default(),
            on,
        })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        return print_answer(&status);
    }

    if !status.host.is_empty() {
        if status.on {
            println!(
                "muru: host {} is in maintenance: clients get 503 maintenance until `muructl maintenance off --host {}`",
                status.host, status.host
            );
        } else {
            println!("muru: host {} serves as usual again", status.host);
        }

        return Ok(());
    }

    if status.on {
        println!(
            "muru: {} is in maintenance: clients get 503 maintenance until `muructl maintenance off {}`",
            status.app, status.app
        );
    } else {
        println!("muru: {} serves as usual again", status.app);
    }

    Ok(())
}

/// The dashboard subcommands.
#[derive(Subcommand)]
pub enum DashboardCommand {
    /// Print a single-use link that signs a browser in to the dashboard.
    Login {
        /// Print a token for scripts to send as `Authorization: Bearer`,
        /// good for twelve hours, instead of a link.
        #[arg(long)]
        token: bool,

        /// The address the dashboard is reached at, when a proxy or tunnel
        /// serves it somewhere other than its own address, such as
        /// https://example.com/proxy/8090/.
        #[arg(long, value_name = "URL", env = "MURU_DASHBOARD_URL")]
        url: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// Run a dashboard subcommand.
pub async fn dashboard(command: DashboardCommand) -> anyhow::Result<()> {
    let DashboardCommand::Login {
        token,
        url,
        socket,
        output,
    } = command;
    let mut client = client::connect(&socket.endpoint()?).await?;
    let link = client
        .dashboard_login(pb::DashboardLoginRequest { token })
        .await
        .map_err(explain)?
        .into_inner();

    if output.json {
        return print_answer(&link);
    }

    if token {
        println!("{}", link.token);
        eprintln!(
            "muru: send it as Authorization: Bearer; it lasts twelve hours, or until the daemon restarts"
        );
        return Ok(());
    }

    match url {
        Some(base) => println!("{}/login?code={}", base.trim_end_matches('/'), link.code),
        None => println!("{}", link.url),
    }

    eprintln!(
        "muru: open the link within {} to sign in; it works once",
        muru_config::format_duration(Duration::from_secs(link.expires_in_s))
    );
    Ok(())
}

/// The virtual host subcommands.
#[derive(Subcommand)]
pub enum HostCommand {
    /// List the virtual hosts.
    List {
        /// Only what this deployment owns.
        #[arg(long, value_name = "NAME")]
        deployment: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// List virtual hosts.
pub async fn host_list(
    socket: SocketArgs,
    json: bool,
    deployment: Option<String>,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let mut answer = client
        .list_hosts(pb::ListHostsRequest {})
        .await?
        .into_inner();
    of_deployment(&mut answer.hosts, deployment.as_deref(), |host| {
        &host.deployment
    });

    if json {
        return print_answer(&answer);
    }

    let hosts = answer.hosts;

    if hosts.is_empty() {
        println!("no virtual hosts");
        return Ok(());
    }

    let owned = names_deployments(hosts.iter().map(|host| host.deployment.as_str()));
    println!(
        "{:<16} {}{:<8} {:<10} NAMES",
        "HOST",
        deployment_cell(owned, "DEPLOYMENT"),
        "TLS",
        "LISTENERS"
    );

    for host in hosts {
        let mut names = host.names.join(", ");

        if host.default {
            names.push_str(if names.is_empty() {
                "(default)"
            } else {
                " (default)"
            });
        }

        // Section 4.12: the edge serves the host's maintenance page.

        if host.maintenance {
            names.push_str(" [maintenance]");
        }

        // Sections 4.3, 4.6 and 11.2: what the host sets for its routes.

        if host.canonical_redirect {
            names.push_str(&format!(" [redirects to {}]", host.canonical));
        }

        if !host.max_body_size.is_empty() {
            names.push_str(&format!(" [max body {}]", host.max_body_size));
        }

        if !host.timeouts.is_empty() {
            names.push_str(&format!(" [{}]", timeouts_note(&host.timeouts)));
        }

        if !host.access_log.is_empty() {
            names.push_str(&format!(" [access log {}]", host.access_log));
        }

        if !host.access_log_file.is_empty() || !host.access_log_format.is_empty() {
            let own = [
                host.access_log_format.as_str(),
                host.access_log_file.as_str(),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" in ");
            names.push_str(&format!(" [access records {own}]"));
        }

        if !host.access_log_query.is_empty() {
            names.push_str(&format!(" [access query {}]", host.access_log_query));
        }

        if !host.compression.is_empty() {
            names.push_str(&format!(" [compression {}]", host.compression));
        }

        // Section 4.7: its rules for headers.

        if !host.request_headers.is_empty() {
            names.push_str(&format!(" [request headers: {}]", host.request_headers));
        }

        if !host.response_headers.is_empty() {
            names.push_str(&format!(" [response headers: {}]", host.response_headers));
        }

        println!(
            "{:<16} {}{:<8} {:<10} {names}",
            host.name,
            deployment_cell(owned, &host.deployment),
            if host.tls { "yes" } else { "" },
            if host.listeners.is_empty() {
                "all".to_owned()
            } else {
                host.listeners.join(",")
            }
        );
    }

    Ok(())
}

/// A host's or a route's own request timeouts as one note, in the order
/// of the keys.
fn timeouts_note(timeouts: &std::collections::BTreeMap<String, String>) -> String {
    timeouts
        .iter()
        .map(|(key, value)| format!("{key} {value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The upstream subcommands (section 4.13).
#[derive(Subcommand)]
pub enum UpstreamCommand {
    /// List the HTTP upstreams.
    List {
        /// Only what this deployment owns.
        #[arg(long, value_name = "NAME")]
        deployment: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// List HTTP upstreams.
pub async fn upstream_list(
    socket: SocketArgs,
    json: bool,
    deployment: Option<String>,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let mut answer = client
        .list_upstreams(pb::ListUpstreamsRequest {})
        .await?
        .into_inner();
    of_deployment(&mut answer.upstreams, deployment.as_deref(), |upstream| {
        &upstream.deployment
    });

    if json {
        return print_answer(&answer);
    }

    let upstreams = answer.upstreams;

    if upstreams.is_empty() {
        println!("no upstreams");
        return Ok(());
    }

    let owned = names_deployments(
        upstreams
            .iter()
            .map(|upstream| upstream.deployment.as_str()),
    );
    println!(
        "{:<16} {}{:<8} {:<6} ADDRESSES",
        "UPSTREAM",
        deployment_cell(owned, "DEPLOYMENT"),
        "PROTOCOL",
        "TLS"
    );

    for upstream in upstreams {
        let mut addresses = upstream.addresses.join(", ");

        if !upstream.health_path.is_empty() {
            addresses.push_str(&format!(" [probing {}]", upstream.health_path));
        }

        if upstream.trust_muru_headers {
            addresses.push_str(" [trusts X-Muru headers]");
        }

        println!(
            "{:<16} {}{:<8} {:<6} {addresses}",
            upstream.name,
            deployment_cell(owned, &upstream.deployment),
            upstream.protocol,
            if upstream.tls { "yes" } else { "" }
        );
    }

    Ok(())
}

/// The route subcommands.
#[derive(Subcommand)]
pub enum RouteCommand {
    /// List the routes.
    List {
        /// Only what this deployment owns.
        #[arg(long, value_name = "NAME")]
        deployment: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// List routes.
pub async fn route_list(
    socket: SocketArgs,
    json: bool,
    deployment: Option<String>,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let mut answer = client
        .list_routes(pb::ListRoutesRequest {})
        .await?
        .into_inner();
    of_deployment(&mut answer.routes, deployment.as_deref(), |route| {
        &route.deployment
    });

    if json {
        return print_answer(&answer);
    }

    let routes = answer.routes;

    if routes.is_empty() {
        println!("no routes");
        return Ok(());
    }

    let owned = names_deployments(routes.iter().map(|route| route.deployment.as_str()));
    println!(
        "{:<16} {:<24} {}TARGET",
        "HOST",
        "PATH",
        deployment_cell(owned, "DEPLOYMENT")
    );

    for route in routes {
        // Section 4.13: an upstream as the target, or behind the files.

        let mut target = if !route.upstream.is_empty() {
            format!("upstream {}", route.upstream)
        } else if route.static_dir.is_empty() {
            route.app
        } else if !route.fallback_upstream.is_empty() {
            format!(
                "static {}, else upstream {}",
                route.static_dir, route.fallback_upstream
            )
        } else if route.fallback_app.is_empty() {
            format!("static {}", route.static_dir)
        } else {
            format!("static {}, else {}", route.static_dir, route.fallback_app)
        };

        // Section 9.4: the policy, when the route has one, with the
        // releases it gives client classes (section 4.14).

        if !route.traffic.is_empty()
            || route.sticky
            || !route.hash_key.is_empty()
            || !route.traffic_classes.is_empty()
        {
            let mut parts: Vec<String> = route
                .traffic
                .iter()
                .map(|share| format!("{} {}%", share.release, share.percent))
                .collect();
            let mut classes: Vec<_> = route.traffic_classes.iter().collect();
            classes.sort();
            parts.extend(
                classes
                    .into_iter()
                    .map(|(class, release)| format!("{class} to {release}")),
            );

            if route.sticky {
                parts.push("sticky".to_owned());
            }

            if !route.hash_key.is_empty() {
                parts.push(format!("by {}", route.hash_key));
            }

            target = format!("{target} ({})", parts.join(", "));
        }

        // Section 4.5: the part of the path the application lives under.

        if !route.strip_prefix.is_empty() {
            target.push_str(&format!(" [strip_prefix {}]", route.strip_prefix));
        }

        // Section 4.10: reachable by internal redirect only, and from
        // which other applications.

        if route.internal {
            target.push_str(" [internal]");
        }

        if !route.accept_redirects_from.is_empty() {
            target.push_str(&format!(
                " [redirects from {}]",
                route.accept_redirects_from.join(", ")
            ));
        }

        // Section 4.16: the handler asked before the route is served.

        if !route.auth.is_empty() {
            target.push_str(&format!(" [auth by {}]", route.auth));
        }

        // Sections 4.6 and 4.11: the route's body limit and uploads.

        if !route.max_body_size.is_empty() {
            target.push_str(&format!(" [max body {}]", route.max_body_size));
        }

        if !route.timeouts.is_empty() {
            target.push_str(&format!(" [{}]", timeouts_note(&route.timeouts)));
        }

        if !route.compression.is_empty() {
            target.push_str(&format!(" [compression {}]", route.compression));
        }

        // Section 4.7: its rules for headers.

        if !route.request_headers.is_empty() {
            target.push_str(&format!(" [request headers: {}]", route.request_headers));
        }

        if !route.response_headers.is_empty() {
            target.push_str(&format!(" [response headers: {}]", route.response_headers));
        }

        if !route.upload.is_empty() {
            target.push_str(&format!(" [uploads {}]", route.upload));
        }

        // Section 4.14: the client classes the route is for.

        if !route.client_classes.is_empty() {
            target.push_str(&format!(" [for {}]", route.client_classes.join(", ")));
        }

        println!(
            "{:<16} {:<24} {}{target}",
            route.host,
            route.path,
            deployment_cell(owned, &route.deployment)
        );
    }

    Ok(())
}

/// The release subcommands.
#[derive(Subcommand)]
pub enum ReleaseCommand {
    /// List releases, of one application or of all.
    List {
        /// The application.
        app: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Show a release: its source and settings, and the mounts and
    /// worker groups it serves.
    Show {
        /// The release, as app@release.
        release: String,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Remove old releases, of one application or of all: their records,
    /// environments, bytecode and logs. Never takes the serving or rollback
    /// release, a running one or one a route or rollout uses. Asks first;
    /// without a terminal it needs --yes.
    Prune(Box<ReleasePruneArgs>),

    /// Remove one release: its record, environment, bytecode and logs.
    /// Refused for the serving or rollback release, a running one or one
    /// a route or rollout uses, and for one applied files declare unless
    /// --force. Asks first; without a terminal it needs --yes.
    Remove(Box<ReleaseRemoveArgs>),

    /// Wait until a release reaches a state.
    Wait {
        /// The release, as app@release.
        release: String,

        /// The states to wait for (default: ready or failed).
        #[arg(long = "state", value_name = "STATE")]
        states: Vec<String>,

        /// How long to wait, in seconds (default 300).
        #[arg(long, default_value = "300")]
        timeout: u64,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// What `muructl release prune` takes.
#[derive(Args, Clone)]
pub struct ReleasePruneArgs {
    /// The application; every application when none is named.
    app: Option<String>,

    /// Keep this many of the newest releases that have not failed, beside
    /// the serving and rollback releases.
    #[arg(long, value_name = "N")]
    keep: u32,

    /// Also take releases that applied files declare, which the next
    /// apply would create again.
    #[arg(long)]
    force: bool,

    /// Leave the log and log archives of what is removed.
    #[arg(long)]
    keep_logs: bool,

    /// Only list what would be removed.
    #[arg(long)]
    dry_run: bool,

    /// Do it without asking.
    #[arg(long)]
    yes: bool,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// What `muructl release remove` takes.
#[derive(Args, Clone)]
pub struct ReleaseRemoveArgs {
    /// The release, as app@release.
    release: String,

    /// Leave its log and log archives.
    #[arg(long)]
    keep_logs: bool,

    /// Remove it although applied files declare it; the next apply of
    /// those files creates it again.
    #[arg(long)]
    force: bool,

    /// Only say what would be removed.
    #[arg(long)]
    dry_run: bool,

    /// Do it without asking.
    #[arg(long)]
    yes: bool,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// List releases.
pub async fn release_list(
    app: Option<String>,
    socket: SocketArgs,
    json: bool,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let answer = client
        .list_releases(pb::ListReleasesRequest {
            app: app.unwrap_or_default(),
            disk: true,
        })
        .await?
        .into_inner();

    if json {
        return print_answer(&answer);
    }

    let releases = answer.releases;

    if releases.is_empty() {
        println!("no releases");
        return Ok(());
    }

    println!(
        "{:<24} {:<10} {:<8} {:<16} {:<13} {:>9} SOURCE",
        "RELEASE", "STATE", "SERVING", "WORKERS", "COMMIT", "DISK"
    );

    for release in releases {
        println!(
            "{:<24} {:<10} {:<8} {:<16} {:<13} {:>9} {}",
            format!("{}@{}", release.app, release.id),
            release.state,
            if release.serving { "yes" } else { "" },
            workers_column(&release),
            commit_column(&release.metadata),
            human_size(release.disk_bytes),
            release.source
        );

        if !release.message.is_empty() {
            println!("{:<24} {}", "", release.message);
        }

        if !release.traffic.is_empty() {
            println!("{:<24} traffic: {}", "", release.traffic);
        }
    }

    Ok(())
}

/// Remove old releases.
pub async fn release_prune(args: ReleasePruneArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let request = pb::PruneReleasesRequest {
        app: args.app.clone().unwrap_or_default(),
        keep: args.keep,
        force: args.force,
        keep_logs: args.keep_logs,
        dry_run: true,
    };

    let plan = client
        .prune_releases(request.clone())
        .await
        .map_err(explain)?
        .into_inner();

    if !show_removal_plan(&plan, args.dry_run, args.output.json)? {
        return Ok(());
    }

    if !confirm_removal(&plan, args.yes)? {
        eprintln!("muru: nothing was changed");
        return Ok(());
    }

    let answer = client
        .prune_releases(pb::PruneReleasesRequest {
            dry_run: false,
            ..request
        })
        .await
        .map_err(explain)?
        .into_inner();

    finish_removal(&answer, args.output.json)
}

/// Remove one release.
pub async fn release_remove(args: ReleaseRemoveArgs) -> anyhow::Result<()> {
    let (app, id) = split_release(&args.release)?;
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let request = pb::RemoveReleaseRequest {
        app: app.to_owned(),
        release: id.to_owned(),
        keep_logs: args.keep_logs,
        dry_run: true,
        force: args.force,
    };

    let plan = client
        .remove_release(request.clone())
        .await
        .map_err(explain)?
        .into_inner();

    if !show_removal_plan(&plan, args.dry_run, args.output.json)? {
        return Ok(());
    }

    if !confirm_removal(&plan, args.yes)? {
        eprintln!("muru: nothing was changed");
        return Ok(());
    }

    let answer = client
        .remove_release(pb::RemoveReleaseRequest {
            dry_run: false,
            ..request
        })
        .await
        .map_err(explain)?
        .into_inner();

    finish_removal(&answer, args.output.json)
}

/// Print a removal's plan. Returns whether it goes on to the question: a
/// dry run stops here, and so does a plan with nothing in it.
fn show_removal_plan(
    plan: &pb::RemoveReleasesResponse,
    dry_run: bool,
    json: bool,
) -> anyhow::Result<bool> {
    if dry_run || plan.removed.is_empty() {
        if json {
            print_answer(plan)?;
            return Ok(false);
        }

        if plan.removed.is_empty() {
            println!("muru: no releases to remove");
        }

        for release in &plan.removed {
            println!("{}", removal_line(release));
        }

        if dry_run {
            for release in &plan.kept {
                println!(
                    "{}@{} is kept ({}): {}",
                    release.app,
                    release.id,
                    release.state,
                    keep_phrase(&release.why)
                );
            }
        }

        if dry_run && !plan.removed.is_empty() {
            println!("would free {}", human_size(plan.freed));
        }

        return Ok(false);
    }

    for release in &plan.removed {
        eprintln!("  {}", removal_line(release));
    }

    Ok(true)
}

/// One release of a removal, for a person: what it is, its state, what
/// removing it frees.
fn removal_line(release: &pb::RemovedRelease) -> String {
    format!(
        "{}@{} ({}, {})",
        release.app,
        release.id,
        release.state,
        human_size(release.disk_bytes)
    )
}

/// Why a release stays, for a person.
fn keep_phrase(why: &str) -> &str {
    match why {
        "serving" => "it serves the application's traffic",
        "rollback" => "it is the release a rollback restores",
        "in_use" => "it is in use",
        "referenced" => "a route's traffic or a rollout under way uses it",
        "declared" => "applied files declare it; --force takes it",
        "keep" => "among the newest kept",
        "failed" => "failed, and not yet older than both the serving and rollback releases",
        other => other,
    }
}

/// The question before a removal.
fn confirm_removal(plan: &pb::RemoveReleasesResponse, yes: bool) -> anyhow::Result<bool> {
    let question = format!(
        "Remove {} release{}, freeing {}?",
        plan.removed.len(),
        if plan.removed.len() == 1 { "" } else { "s" },
        human_size(plan.freed)
    );

    confirm(&question, yes)
}

/// Report a removal and fail if any part of it did.
fn finish_removal(answer: &pb::RemoveReleasesResponse, json: bool) -> anyhow::Result<()> {
    if json {
        print_answer(answer)?;
    } else {
        println!("muru: removed, freeing {}", human_size(answer.freed));
    }

    let failures: Vec<String> = answer
        .removed
        .iter()
        .filter(|release| !release.error.is_empty())
        .map(|release| format!("{}@{}: {}", release.app, release.id, release.error))
        .collect();

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(failures.join("; "))
    }
}

/// The WORKERS column of `release list`: what runs now, or `-` when
/// nothing does, and the shape the release asks for in brackets.
fn workers_column(release: &pb::Release) -> String {
    if let Some(detour) = &release.detour {
        return format!("- (detour, pid {})", detour.pid);
    }

    if release.kind == "http" {
        let running = release
            .server_instances
            .iter()
            .filter(|instance| instance.pid != 0)
            .count();
        let shape = format!("{} {}", release.instances, release.profile);

        return match running {
            0 => format!("- ({shape})"),
            count => format!("{count} ({shape})"),
        };
    }

    let shape = format!("{}x{}", release.processes, release.threads);

    if release.host_running {
        format!("{} ({shape})", release.workers.len())
    } else {
        format!("- ({shape})")
    }
}

/// Show one release.
/// A release's or a group's `max_surge` and `max_unavailable` in words,
/// with the order its swaps take.
fn swap_settings(max_surge: u32, max_unavailable: u32) -> String {
    let order = if max_surge == 0 {
        "the old worker stops first"
    } else {
        "the new worker starts first"
    };

    format!("max_surge {max_surge}, max_unavailable {max_unavailable} ({order})")
}

/// What a release's description says of a surge allowance the daemon
/// granted short of `max_surge`, or nothing when it is whole or no host
/// runs.
fn surge_granted(max_surge: u32, allowance: Option<u32>, short: &str) -> String {
    let Some(allowance) = allowance.filter(|allowance| *allowance < max_surge) else {
        return String::new();
    };

    let why = match short {
        "traffic" => "the release takes no traffic".to_owned(),
        "workers" => "the node's worker budget has no room".to_owned(),
        "memory" => "the node's memory budget has no room".to_owned(),
        "cpu" => "the node's CPU budget has no room".to_owned(),
        _ => "the node's budget has no room".to_owned(),
    };

    if allowance == 0 && short == "traffic" {
        format!("; no surge allowance now, since {why}, so the old worker stops first")
    } else if allowance == 0 {
        format!("; no surge allowance now, since {why}")
    } else {
        format!("; a surge allowance of {allowance} now, since {why}")
    }
}

pub async fn release_show(release: String, socket: SocketArgs, json: bool) -> anyhow::Result<()> {
    let (app, id) = split_release(&release)?;
    let mut client = client::connect(&socket.endpoint()?).await?;
    let release = client
        .get_release(pb::GetReleaseRequest {
            app: app.to_owned(),
            release: id.to_owned(),
        })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        return print_answer(&release);
    }

    for (key, value) in release_details(&release) {
        println!("{key:<12} {value}");
    }

    Ok(())
}

/// The commit a release's metadata names, as `release list` shows it
/// (section 9.14): shortened, with `+` when its checkout had changes, or
/// `-` for none.
fn commit_column(metadata: &std::collections::BTreeMap<String, String>) -> String {
    muru_config::commit_label(metadata).unwrap_or_else(|| "-".to_owned())
}

/// A release's details as the lines `release show` prints, key first.
fn release_details(release: &pb::Release) -> Vec<(String, String)> {
    let mut lines = vec![
        (
            "release".to_owned(),
            format!("{}@{}", release.app, release.id),
        ),
        (
            "state".to_owned(),
            if release.serving {
                format!("{}, serving", release.state)
            } else {
                release.state.clone()
            },
        ),
    ];

    if !release.message.is_empty() {
        lines.push(("message".to_owned(), release.message.clone()));
    }

    // Section 13.8: the number that names its host's directories.

    if release.number > 0 {
        lines.push(("number".to_owned(), release.number.to_string()));
    }

    lines.push(("source".to_owned(), release.source.clone()));

    // Section 9.14: every key of its metadata, the well-known ones first.

    for (key, value) in metadata_in_order(&release.metadata) {
        lines.push((key.to_owned(), value.replace('\n', " ")));
    }

    if release.websockets > 0 {
        lines.push((
            "websockets".to_owned(),
            format!("{} open", release.websockets),
        ));
    }

    // Section 9.13: a detour is a development run's, which the daemon
    // only routes to.

    if let Some(detour) = &release.detour {
        lines.push(("kind".to_owned(), "detour".to_owned()));
        lines.push((
            "traffic".to_owned(),
            muru_controller::detours::listed_traffic(
                &detour.traffic,
                detour.until_unix_ms,
                &detour.paths,
            ),
        ));
        lines.push(("pid".to_owned(), detour.pid.to_string()));

        if !detour.label.is_empty() {
            lines.push(("label".to_owned(), detour.label.clone()));
        }

        lines.push(("socket".to_owned(), detour.socket.clone()));
        return lines;
    }

    if release.kind == "http" {
        lines.push((
            "server".to_owned(),
            format!("{}, {} instances", release.profile, release.instances),
        ));
        lines.push(("command".to_owned(), release.command.join(" ")));

        for instance in &release.server_instances {
            let process = if instance.pid == 0 {
                "waiting to start again".to_owned()
            } else {
                format!("pid {}", instance.pid)
            };
            lines.push((
                format!("instance {}", instance.index),
                format!("{process}, {}", instance.address),
            ));
        }

        return lines;
    }

    // Which sizes apply follows from the interfaces: threads for WSGI,
    // the event loop for ASGI and gRPC, both while one is not yet known.

    let interfaces: Vec<&str> =
        std::iter::once(effective(&release.interface, &release.detected_interface))
            .chain(
                release
                    .mounts
                    .iter()
                    .map(|mount| effective(&mount.interface, &mount.detected_interface)),
            )
            .collect();
    let uses = Uses {
        threads: interfaces
            .iter()
            .any(|interface| matches!(*interface, "wsgi" | "auto")),
        event_loop: interfaces.iter().any(|interface| *interface != "wsgi"),
    };

    let mut python = release.python.clone();

    if !release.python_version.is_empty() {
        python.push_str(&format!(
            " ({}, {})",
            release.python_version, release.python_tag
        ));
    }

    if !release.python_request.is_empty() {
        python.push_str(&format!(", requested {}", release.python_request));
    }

    lines.push(("python".to_owned(), python));

    if !release.environment.is_empty() {
        lines.push((
            "environment".to_owned(),
            format!(
                "{} (built, build.when = {})",
                release.environment, release.build_when
            ),
        ));
    }

    if !release.bytecode.is_empty() {
        lines.push(("bytecode".to_owned(), release.bytecode.clone()));
    }

    lines.push((
        "entrypoint".to_owned(),
        format!(
            "{}, {}",
            release.entrypoint,
            interface(&release.interface, &release.detected_interface)
        ),
    ));
    lines.push((
        "workers".to_owned(),
        group_size(
            uses,
            release.processes,
            release.threads,
            release.concurrency,
            release.sync_threads,
        ) + &initial_note(release.initial_processes),
    ));

    for group in &release.groups {
        let mut serves = group.paths.join(", ");

        if !group.client_classes.is_empty() {
            if !serves.is_empty() {
                serves.push_str(" and ");
            }

            serves.push_str(&format!(
                "client classes {}",
                group.client_classes.join(", ")
            ));
        }

        serves.push_str(&group_asks(&group.hosts, &group.methods));
        serves.push_str(&group_mounts(&group.mounts));

        lines.push((
            format!("group {}", group.name),
            format!(
                "{serves}: {}{}",
                group_size(
                    uses,
                    group.processes,
                    group.threads,
                    group.concurrency,
                    group.sync_threads
                ) + &initial_note(group.initial_processes),
                if group.optional { ", optional" } else { "" }
            ) + &if (group.max_surge, group.max_unavailable)
                == (release.max_surge, release.max_unavailable)
            {
                String::new()
            } else {
                format!(
                    ", {}",
                    swap_settings(group.max_surge, group.max_unavailable)
                )
            } + &surge_granted(group.max_surge, group.surge_allowance, &group.surge_short),
        ));
    }

    // Section 7.8: how far a group may move from its count while its
    // workers are swapped. A release of another kind has neither.

    if release.max_surge + release.max_unavailable > 0 {
        lines.push((
            "swaps".to_owned(),
            swap_settings(release.max_surge, release.max_unavailable)
                + &surge_granted(
                    release.max_surge,
                    release.surge_allowance,
                    &release.surge_short,
                ),
        ));
    }

    // Section 10.14: a restart of every worker under way.

    if let Some(restart) = &release.restart {
        lines.push((
            "restart".to_owned(),
            format!(
                "{} of {}{} still to be replaced, each in its turn; muructl restart --cancel \
                 leaves them",
                restart.left,
                counted(restart.total, "worker", "workers"),
                if restart.group.is_empty() {
                    String::new()
                } else {
                    format!(" of the group {}", restart.group)
                }
            ),
        ));
    }

    for mount in &release.mounts {
        let mut serves = if mount.path.is_empty() {
            format!("the gRPC services of {}", mount.entrypoint)
        } else {
            format!("{} from {}", mount.path, mount.entrypoint)
        };
        serves.push_str(&format!(
            ", {}",
            interface(&mount.interface, &mount.detected_interface)
        ));

        if mount.threads > 0 && mount.max_threads > mount.threads {
            serves.push_str(&format!(
                ", {} threads of its own, borrowing shared ones up to {}",
                mount.threads, mount.max_threads
            ));
        } else if mount.threads > 0 {
            serves.push_str(&format!(", {} threads of its own", mount.threads));
        }

        if mount.internal {
            serves.push_str(", to internal redirects only");
        }

        lines.push((format!("mount {}", mount.name), serves));
    }

    for gate in &release.gates {
        lines.push((
            format!("gate {}", gate.name),
            format!(
                "{} passed {}, run {}",
                gate.stage,
                format_time(gate.passed_unix_ms),
                gate.run
            ),
        ));
    }

    lines
}

/// The interface an entrypoint is served with: as declared, or as
/// detected once the host has settled it; `auto` until then.
fn effective<'a>(declared: &'a str, detected: &'a str) -> &'a str {
    if declared == "auto" && !detected.is_empty() {
        detected
    } else {
        declared
    }
}

/// An entrypoint's interface as `release show` names it.
fn interface(declared: &str, detected: &str) -> String {
    let name = |interface: &str| match interface {
        "grpc" => "gRPC".to_owned(),
        other => other.to_uppercase(),
    };

    match (declared, detected) {
        ("auto", "") => "interface auto, not yet detected".to_owned(),
        ("auto", detected) => format!("{} (detected)", name(detected)),
        (declared, _) => name(declared),
    }
}

/// Which of a release's sizes its workers use.
#[derive(Clone, Copy)]
struct Uses {
    /// Application threads, for WSGI.
    threads: bool,
    /// The event loop, for ASGI and gRPC.
    event_loop: bool,
}

/// What `release show` adds to a group's size when the release was
/// deployed with fewer workers to start with (issue 964).
fn initial_note(initial: Option<u32>) -> String {
    initial.map_or_else(String::new, |initial| {
        format!("; {initial} to start with and while it takes no traffic")
    })
}

/// A group's processes and what each takes at once: threads for WSGI,
/// the event loop's concurrency and sync threads for ASGI.
/// What a group asks of a request beside its path and class (ADR
/// 0033): the hosts and methods it takes, as a suffix for its line.
pub fn group_asks(hosts: &[String], methods: &[String]) -> String {
    let mut asks = String::new();

    if !hosts.is_empty() {
        asks.push_str(&format!(
            " on host{} {}",
            if hosts.len() == 1 { "" } else { "s" },
            hosts.join(", ")
        ));
    }

    if !methods.is_empty() {
        asks.push_str(&format!(
            " with method{} {}",
            if methods.len() == 1 { "" } else { "s" },
            methods.join(", ")
        ));
    }

    asks
}

/// The named mounts a group's workers serve beside the default mount
/// (ADR 0033), as a suffix for its line; empty for every mount.
pub fn group_mounts(mounts: &[String]) -> String {
    if mounts.is_empty() {
        return String::new();
    }

    format!(
        " serving mount{} {}",
        if mounts.len() == 1 { "" } else { "s" },
        mounts.join(", ")
    )
}

fn group_size(
    uses: Uses,
    processes: u32,
    threads: u32,
    concurrency: u32,
    sync_threads: u32,
) -> String {
    let processes = counted(processes, "process", "processes");
    let threads = counted(threads, "thread", "threads");
    let event_loop = format!(
        "an event loop of concurrency {concurrency} with {}",
        counted(sync_threads, "sync thread", "sync threads")
    );

    match (uses.threads, uses.event_loop) {
        (true, true) => format!("{processes} of {threads} and {event_loop}"),
        (false, true) => format!("{processes} of {event_loop}"),
        _ => format!("{processes} of {threads}"),
    }
}

/// A count with its noun, singular for one.
pub fn counted<T>(count: T, singular: &str, plural: &str) -> String
where
    T: std::fmt::Display + PartialEq + From<u8>,
{
    if count == T::from(1) {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// Wait for a release to reach a state.
pub async fn release_wait(
    release: String,
    states: Vec<String>,
    timeout: u64,
    socket: SocketArgs,
    json: bool,
) -> anyhow::Result<()> {
    let (app, id) = split_release(&release)?;
    let mut client = client::connect(&socket.endpoint()?).await?;
    let release = client
        .wait_release(pb::WaitReleaseRequest {
            app: app.to_owned(),
            release: id.to_owned(),
            states,
            timeout_ms: timeout * 1000,
        })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        return print_answer(&release);
    }

    println!(
        "muru: release {}@{} is {}",
        release.app, release.id, release.state
    );

    if !release.message.is_empty() {
        println!("muru: {}", release.message);
    }

    Ok(())
}

/// Roll an application back to its previous release.
pub async fn rollback(app: String, socket: SocketArgs, json: bool) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let release = client
        .rollback(pb::RollbackRequest { app })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        return print_answer(&release);
    }

    println!(
        "muru: rolled back to {}@{} ({})",
        release.app, release.id, release.state
    );
    Ok(())
}

/// Drain a release.
pub async fn drain(release: String, socket: SocketArgs, json: bool) -> anyhow::Result<()> {
    let (app, id) = split_release(&release)?;
    let mut client = client::connect(&socket.endpoint()?).await?;
    let release = client
        .drain_release(pb::DrainReleaseRequest {
            app: app.to_owned(),
            release: id.to_owned(),
        })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        return print_answer(&release);
    }

    println!(
        "muru: release {}@{} is {}",
        release.app, release.id, release.state
    );
    Ok(())
}

/// What `apply` takes.
#[derive(Args)]
pub struct ApplyArgs {
    /// A file to apply, or a directory whose .toml files are applied
    /// together. Given more than once, the files are applied as one.
    #[arg(
        short = 'f',
        long = "file",
        value_name = "FILE or DIR",
        required = true
    )]
    file: Vec<PathBuf>,

    /// Report what would change and change nothing.
    #[arg(long)]
    dry_run: bool,

    /// Also remove what the files do not contain, of the deployments
    /// they name; with --prune=all, of every deployment, so the files
    /// are the whole of what the daemon serves.
    #[arg(
        long,
        value_name = "REACH",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "files"
    )]
    prune: Option<PruneScope>,

    /// Deploy or promote releases of applications an overlay freezes;
    /// recorded as an event.
    #[arg(long)]
    override_freeze: bool,

    /// Start the releases the apply adds but leave traffic where it is,
    /// for a preview with muructl preview or a rollout; refused
    /// when an application's serving key would move its traffic.
    #[arg(long)]
    hold: bool,

    /// Return as soon as the releases the apply adds are recorded,
    /// without waiting for them to serve.
    #[arg(long)]
    no_wait: bool,

    /// How long a release the apply replaces stays warm for an instant
    /// rollback (default 30m; 0 stops it as soon as it has drained).
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    retain: Option<Duration>,

    /// The workers each group of a release the apply replaces keeps
    /// while it stays warm (default 1, or 0 for a group whose
    /// min_workers is 0; 0 keeps the host alone).
    #[arg(long, value_name = "N")]
    retain_workers: Option<u32>,

    /// Print the report as JSON.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    socket: SocketArgs,
}

/// Apply declarative configuration. The exit code is 0 when nothing
/// changed or everything applied, 2 for a dry run with changes planned
/// and 1 for errors.
pub async fn apply(args: ApplyArgs) -> anyhow::Result<i32> {
    let documents = load_documents(&args.file)?;
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let report = client
        .apply_config(pb::ApplyConfigRequest {
            documents,
            document: String::new(),
            prune_all: args.prune == Some(PruneScope::All),
            dry_run: args.dry_run,
            prune: args.prune.is_some(),
            override_freeze: args.override_freeze,
            hold: args.hold,
            no_wait: args.no_wait,
            retain_ms: args.retain.map(|retain| retain.as_millis() as u64),
            retain_workers: args.retain_workers,
            remove_data: false,
        })
        .await
        .map_err(explain)?
        .into_inner();

    if args.json {
        print_answer(&report)?;
    } else {
        print_report(&report, args.dry_run);

        if args.hold && !args.dry_run && !args.no_wait {
            eprintln!("muru: muructl preview APP@RELEASE opens a preview of a held release");
        }
    }

    Ok(exit_code(&report, args.dry_run))
}

/// The configuration subcommands.
#[derive(Subcommand)]
pub enum DeploymentCommand {
    /// List the deployments: the named sets of applications, hosts and
    /// routes this daemon serves, each applied, pruned and removed
    /// together. default holds what files with no deployment key and
    /// commands made.
    List {
        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Remove a deployment as a unit: drain what it runs and remove its
    /// applications with their releases, its hosts, routes and the rest,
    /// after listing what goes and asking. Refused while another
    /// deployment names something of it. default is emptied and kept.
    /// Application data directories are kept unless --data.
    Remove {
        /// The deployment.
        name: String,

        /// Also remove the data directories of its applications.
        #[arg(long)]
        data: bool,

        /// Only list what would be removed.
        #[arg(long)]
        dry_run: bool,

        /// Do it without asking.
        #[arg(long)]
        yes: bool,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Write desired state as a file that applies back without changes.
    Export {
        /// The deployment to write, with its deployment key at the top.
        /// With none, a daemon whose files name no deployment writes
        /// everything, as default.
        #[arg(conflicts_with_all = ["dir", "app"])]
        name: Option<String>,

        /// Write a file for each deployment into this directory, which
        /// must be empty: NAME.toml, which muructl apply -f DIR applies
        /// back without changes.
        #[arg(long, value_name = "DIR", conflicts_with = "app")]
        dir: Option<PathBuf>,

        /// One application, with the routes that reach it and their
        /// hosts, whichever deployment each is in.
        #[arg(long, value_name = "NAME")]
        app: Option<String>,

        /// With --app: write this deployment's name at the top, for a
        /// file that moves the application out of default.
        #[arg(long = "as", value_name = "DEPLOYMENT", requires = "app")]
        as_deployment: Option<String>,

        /// Write each rollout under way as where it stands: its candidate
        /// declared, and the application's routes splitting traffic at
        /// its current share.
        #[arg(long)]
        resolve_rollouts: bool,

        /// Write JSON rather than TOML.
        #[arg(long)]
        json: bool,

        #[command(flatten)]
        socket: SocketArgs,
    },

    /// Show desired state; with --effective, as the active overlays leave
    /// it, with what each sets that is in force.
    Show {
        /// One deployment rather than everything, as its files would
        /// state it.
        name: Option<String>,

        /// One application rather than all.
        #[arg(long, value_name = "APP", conflicts_with = "name")]
        app: Option<String>,

        /// Apply the active overlays and attribute what they set.
        #[arg(long)]
        effective: bool,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Compare a file, or a directory of files, with desired state.
    Diff {
        /// A file, or a directory whose .toml files are read together.
        /// Given more than once, the files are read as one.
        #[arg(
            short = 'f',
            long = "file",
            value_name = "FILE or DIR",
            required = true
        )]
        file: Vec<PathBuf>,

        /// Print the report as JSON.
        #[arg(long)]
        json: bool,

        #[command(flatten)]
        socket: SocketArgs,
    },
}

/// Export desired state, with a header naming where it came from.
/// What `deployment export` writes.
pub struct Exporting {
    /// One deployment.
    pub name: Option<String>,
    /// A file for each deployment, into this directory.
    pub dir: Option<PathBuf>,
    /// One application.
    pub app: Option<String>,
    /// The deployment key to write with `app`.
    pub as_deployment: Option<String>,
    /// Each rollout under way as where it stands.
    pub resolve_rollouts: bool,
    /// JSON rather than TOML.
    pub json: bool,
}

/// Write desired state as files that apply back without changes: one
/// deployment, one application, or with `dir` a file for each deployment.
pub async fn config_export(exporting: Exporting, socket: SocketArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let named: Vec<String> = client
        .list_deployments(pb::ListDeploymentsRequest {})
        .await
        .map_err(explain)?
        .into_inner()
        .deployments
        .into_iter()
        .filter(|deployment| {
            deployment.name != muru_controller::deployments::DEFAULT
                || !deployment.apps.is_empty()
                || deployment.hosts + deployment.routes + deployment.others > 0
        })
        .map(|deployment| deployment.name)
        .collect();
    let several = named
        .iter()
        .any(|name| name != muru_controller::deployments::DEFAULT);

    // One file cannot say which deployment each of its tables belongs
    // to, so a daemon with named deployments is exported one at a time,
    // or into a directory.

    if exporting.name.is_none() && exporting.dir.is_none() && exporting.app.is_none() && several {
        anyhow::bail!(
            "this daemon has the deployments {}; name one, as in muructl deployment export {}, \
             or write a file for each with --dir DIR",
            named.join(", "),
            named[0]
        );
    }

    let extension = if exporting.json { "json" } else { "toml" };
    let mut fetch = async |deployment: Option<&str>| -> anyhow::Result<String> {
        let export = client
            .export_config(pb::ExportConfigRequest {
                app: exporting.app.clone().unwrap_or_default(),
                effective: false,
                resolve_rollouts: exporting.resolve_rollouts,
                deployment: deployment.unwrap_or_default().to_owned(),
                as_deployment: exporting.as_deployment.clone().unwrap_or_default(),
            })
            .await
            .map_err(explain)?
            .into_inner();

        if exporting.json {
            let document =
                muru_config::Document::parse(&export.document).map_err(anyhow::Error::msg)?;
            return Ok(serde_json::to_string_pretty(&document)? + "\n");
        }

        Ok(format!(
            "# Muru desired state: muructl {} on {}, {}, state version {}.\n\
             # Applying this file to the daemon it came from changes nothing.\n\n{}",
            export.version,
            export.node,
            format_time(export.time_unix_ms),
            export.state_version,
            export.document
        ))
    };

    let Some(dir) = &exporting.dir else {
        print!("{}", fetch(exporting.name.as_deref()).await?);
        return Ok(());
    };

    // A file left from a deployment since removed would be applied back
    // with the rest, so the directory starts empty.

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    if std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .next()
        .is_some()
    {
        anyhow::bail!(
            "{} is not empty; a file there from a deployment since removed would be applied \
             back with the rest, so give an empty directory",
            dir.display()
        );
    }

    if named.is_empty() {
        println!("muru: there is nothing to export");
        return Ok(());
    }

    for name in &named {
        let path = dir.join(format!("{name}.{extension}"));
        std::fs::write(&path, fetch(Some(name)).await?)
            .with_context(|| format!("writing {}", path.display()))?;
        println!("{}", path.display());
    }

    Ok(())
}

/// Show desired state, or with `effective` the state the active overlays
/// make of it, headed by what each sets that is in force.
pub async fn config_show(
    deployment: Option<String>,
    app: Option<String>,
    effective: bool,
    socket: SocketArgs,
    json: bool,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let export = client
        .export_config(pb::ExportConfigRequest {
            app: app.unwrap_or_default(),
            deployment: deployment.unwrap_or_default(),
            effective,
            resolve_rollouts: false,
            ..pb::ExportConfigRequest::default()
        })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        return print_answer(&export);
    }

    if !effective {
        print!("{}", export.document);
        return Ok(());
    }

    println!(
        "# Muru effective state: muructl {} on {}, {}, state version {}.",
        export.version,
        export.node,
        format_time(export.time_unix_ms),
        export.state_version
    );

    if export.attributions.is_empty() {
        println!("# No active overlay changes this configuration.\n");
    } else {
        println!("# The active overlays set, and the routes below include:");

        for line in &export.attributions {
            println!("#   {line}");
        }

        println!();
    }

    if !export.layers.is_empty() {
        println!("# The releases' settings and the routes' limits, and the layer each came from:");

        for line in &export.layers {
            println!("#   {line}");
        }

        println!();
    }

    print!("{}", export.document);
    Ok(())
}

/// List the deployments with what each holds.
pub async fn deployment_list(socket: SocketArgs, json: bool) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let answer = client
        .list_deployments(pb::ListDeploymentsRequest {})
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        return print_answer(&answer);
    }

    println!(
        "{:<24} {:<32} {:>5} {:>6} {:>6}",
        "NAME", "APPS", "HOSTS", "ROUTES", "OTHER"
    );

    for deployment in &answer.deployments {
        let apps = if deployment.apps.is_empty() {
            "-".to_owned()
        } else {
            deployment.apps.join(", ")
        };
        println!(
            "{:<24} {:<32} {:>5} {:>6} {:>6}",
            deployment.name, apps, deployment.hosts, deployment.routes, deployment.others
        );
    }

    Ok(())
}

/// Remove a deployment as a unit: what an apply of no files for it, with
/// a prune, removes. The exit code is 0 when it went or there was nothing
/// to remove, and 1 for a refusal or a failure.
pub async fn deployment_remove(
    name: String,
    data: bool,
    dry_run: bool,
    yes: bool,
    socket: SocketArgs,
) -> anyhow::Result<i32> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let known = client
        .list_deployments(pb::ListDeploymentsRequest {})
        .await
        .map_err(explain)?
        .into_inner()
        .deployments;

    if !known.iter().any(|deployment| deployment.name == name) {
        anyhow::bail!("there is no deployment {name}; muructl deployment list shows them");
    }

    // A deployment's files with nothing in them, pruned: everything it
    // owns goes, and nothing of any other deployment.

    let document = if name == muru_controller::deployments::DEFAULT {
        String::new()
    } else {
        format!("deployment = \"{name}\"\n")
    };
    let request = pb::ApplyConfigRequest {
        documents: vec![document],
        prune: true,
        dry_run: true,
        remove_data: data,
        ..pb::ApplyConfigRequest::default()
    };
    let plan = client
        .apply_config(request.clone())
        .await
        .map_err(explain)?
        .into_inner();

    if plan.changes.is_empty() && plan.errors.is_empty() {
        println!("muru: the deployment {name} holds nothing");
        return Ok(0);
    }

    print_report(&plan, true);

    if !plan.errors.is_empty() {
        return Ok(1);
    }

    if dry_run {
        return Ok(0);
    }

    if !confirm(&format!("Remove the deployment {name}?"), yes)? {
        eprintln!("muru: nothing was changed");
        return Ok(0);
    }

    let report = client
        .apply_config(pb::ApplyConfigRequest {
            dry_run: false,
            ..request
        })
        .await
        .map_err(explain)?
        .into_inner();
    print_report(&report, false);
    Ok(exit_code(&report, false))
}

/// Compare a file with desired state. The exit code is 0 when they
/// match, 2 when they differ and 1 for errors.
pub async fn config_diff(
    file: Vec<PathBuf>,
    json: bool,
    socket: SocketArgs,
) -> anyhow::Result<i32> {
    let documents = load_documents(&file)?;
    let mut client = client::connect(&socket.endpoint()?).await?;
    let report = client
        .diff_config(pb::DiffConfigRequest {
            documents,
            document: String::new(),
        })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        print_answer(&report)?;
    } else {
        print_report(&report, true);
    }
    Ok(exit_code(&report, true))
}

/// Read one file, or every .toml file of a directory in name order, into
/// one document, with relative source paths taken from each file's
/// directory.
/// Make every path in an applied document absolute against `base`, the
/// directory of the file it came from: a release's source, interpreter
/// and file hint roots, a host's certificate and key, a route's or an
/// overlay's static directory, the error page files of hosts and routes,
/// an upstream's trusted certificates and a client class's lists. A
/// release's error page files are the exception: they are relative to
/// its source, where the daemon checks them. (The node-wide tables
/// belong to the daemon's own file, which the apply leaves alone.)
fn resolve_paths(document: &mut muru_config::Document, base: &Path) {
    let absolute = |path: &str| {
        if Path::new(path).is_relative() {
            base.join(path).display().to_string()
        } else {
            path.to_owned()
        }
    };
    let resolve = |path: &mut Option<String>| {
        if let Some(value) = path {
            *value = absolute(value);
        }
    };
    let resolve_pages = |pages: &mut std::collections::BTreeMap<String, muru_config::ErrorPage>| {
        for page in pages.values_mut() {
            resolve(&mut page.file);
        }
    };

    for app in document.app.values_mut() {
        for release in app.release.values_mut() {
            resolve(&mut release.source);
            resolve(&mut release.cwd);

            if let Some(python) = &release.python {
                release.python = Some(absolute_interpreter(Path::new(python), Some(base)));
            }

            // A release's sendfile_roots, its mounts' among them, are
            // taken from its source rather than the file, as its error
            // pages are, so they mean the same in every release.
        }
    }

    for host in document.host.values_mut() {
        if let Some(tls) = &mut host.tls {
            tls.certificate = absolute(&tls.certificate);
            tls.key = absolute(&tls.key);
        }

        resolve_pages(&mut host.error_pages);
    }

    for route in &mut document.route {
        resolve(&mut route.static_dir);
        resolve_pages(&mut route.error_pages);
    }

    for overlay in document.overlay.values_mut() {
        for route in overlay.routes.values_mut() {
            resolve_route_overlay(route, &absolute);
        }
    }

    for upstream in document.upstream.values_mut() {
        if let Some(tls) = &mut upstream.tls {
            resolve(&mut tls.ca);
        }

        for root in &mut upstream.sendfile_roots {
            *root = absolute(root);
        }
    }

    for class in &mut document.client_class {
        resolve(&mut class.user_agent_list);
        resolve(&mut class.address_list);
        resolve(&mut class.ja4_list);
    }
}

/// The paths of one route overlay and of the client routes under it.
fn resolve_route_overlay(route: &mut muru_config::RouteOverlay, absolute: &dyn Fn(&str) -> String) {
    if let Some(target) = &mut route.target
        && let Some(directory) = &target.static_dir
    {
        target.static_dir = Some(absolute(directory));
    }

    if let Some(pages) = &mut route.error_pages {
        for page in pages.values_mut() {
            if let Some(file) = &page.file {
                page.file = Some(absolute(file));
            }
        }
    }

    if let Some(routes) = &mut route.client_routes {
        for route in routes.values_mut() {
            resolve_route_overlay(route, absolute);
        }
    }
}

/// The files `-f` names, read as one document: each file as it is, and
/// of a directory its .toml files in name order.
fn load_documents(paths: &[PathBuf]) -> anyhow::Result<Vec<String>> {
    let mut files = Vec::new();

    for path in paths {
        if !path.is_dir() {
            files.push(path.clone());
            continue;
        }

        // A project's directory is not a directory of files to apply:
        // its pyproject.toml or muru.toml holds the project's settings,
        // and failed here on its first key (issue 931).

        if let Some(project) = ["pyproject.toml", "muru.toml"]
            .iter()
            .find(|name| path.join(name).is_file())
        {
            anyhow::bail!(
                "{} is a project (it has a {project}), not a directory of files to apply; name \
                 the file, as in muructl apply -f {}",
                path.display(),
                path.join(muru_config::DEPLOY_FILE).display()
            );
        }

        let mut found = Vec::new();

        for entry in
            std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))?
        {
            let entry = entry?.path();

            if entry
                .extension()
                .is_some_and(|extension| extension == "toml")
            {
                found.push(entry);
            }
        }

        if found.is_empty() {
            anyhow::bail!("{} holds no .toml files", path.display());
        }

        found.sort();
        files.extend(found);
    }

    // Files are read a deployment at a time: those with the same
    // `deployment` key, or with none, are one document (ADR 0080).

    let mut deployments: std::collections::BTreeMap<String, muru_config::Document> =
        std::collections::BTreeMap::new();

    for file in files {
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let mut parsed = muru_config::Document::parse(&text)
            .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))?;

        // The daemon's own tables are no part of what an apply changes
        // (issue 862). They are refused here, by file, since folding the
        // files into one document keeps only some of them for the daemon
        // to refuse.

        let daemons = parsed.daemon_tables();

        if !daemons.is_empty() {
            let lines: Vec<String> = daemons
                .iter()
                .map(|table| {
                    format!(
                        "{}: {table} is part of the daemon's own configuration, which muructl \
                         apply does not change: put it in the daemon's configuration file and \
                         run muructl daemon reload",
                        file.display()
                    )
                })
                .collect();
            anyhow::bail!("{}", lines.join("\n"));
        }

        // Paths are resolved here, where the file is, since the daemon
        // may run elsewhere. The file's directory is made absolute first,
        // so that a file named by a relative path gives absolute paths
        // too: the daemon checks some of them, such as sendfile_roots,
        // for being absolute.

        let base = file
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let base = std::fs::canonicalize(&base).unwrap_or_else(|_| {
            std::env::current_dir().map_or(base.clone(), |cwd| cwd.join(&base))
        });
        resolve_paths(&mut parsed, &base);
        deployments
            .entry(parsed.deployment.clone().unwrap_or_default())
            .or_default()
            .merge(parsed)
            .map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))?;
    }

    deployments
        .into_values()
        .map(|document| document.to_toml().context("writing the configuration"))
        .collect()
}

/// How far `--prune` reaches: the deployments the files name, which is
/// the flag alone, or every deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PruneScope {
    /// The deployments the files name.
    Files,
    /// Every deployment, so the files are the whole of what is served.
    All,
}

impl PruneScope {
    /// The flag that asks for it.
    fn flag(self) -> &'static str {
        match self {
            PruneScope::Files => "--prune",
            PruneScope::All => "--prune=all",
        }
    }

    /// The reach a flag asks for, none without it.
    fn reach(scope: Option<PruneScope>) -> muru_controller::deployments::Prune {
        match scope {
            None => muru_controller::deployments::Prune::No,
            Some(PruneScope::Files) => muru_controller::deployments::Prune::Files,
            Some(PruneScope::All) => muru_controller::deployments::Prune::All,
        }
    }
}

/// Print a report as people read it.
fn print_report(report: &pb::ConfigReport, dry_run: bool) {
    // Section 10.10: an apply that stopped in its prepare phase says why
    // and what became of each change.

    let stopped = report
        .changes
        .iter()
        .any(|change| !change.outcome.is_empty());

    if !report.errors.is_empty() && stopped {
        println!("muru: nothing applied:");

        for error in &report.errors {
            println!("  {error}");
        }

        for change in &report.changes {
            println!(
                "  {:<15} {:<24} {}",
                change.action, change.subject, change.outcome
            );
        }
    } else if !report.errors.is_empty() {
        println!("muru: nothing applied; the configuration has errors:");

        for error in &report.errors {
            println!("  {error}");
        }
    } else if report.changes.is_empty() {
        println!(
            "muru: no changes ({} object{} as declared)",
            report.unchanged,
            if report.unchanged == 1 { "" } else { "s" }
        );
    } else {
        println!(
            "muru: {}",
            if report.applied {
                "applied"
            } else if dry_run {
                "plan (nothing changed)"
            } else {
                "plan"
            }
        );

        for change in &report.changes {
            let what = if report.applied {
                change.outcome.as_str()
            } else {
                change.detail.as_str()
            };
            println!("  {:<15} {:<24} {what}", change.action, change.subject);
        }

        if report.unchanged > 0 {
            println!(
                "  {} object{} as declared",
                report.unchanged,
                if report.unchanged == 1 { "" } else { "s" }
            );
        }
    }

    for note in &report.notes {
        println!("muru: {note}");
    }

    for warning in &report.warnings {
        println!("muru: warning: {warning}");
    }

    if !report.snapshot_error.is_empty() {
        println!(
            "muru: the changes were made, but the edge did not take the new snapshot: {}",
            report.snapshot_error
        );
    }
}

/// Section 10.4: 0 for no changes, 2 for changes planned, 1 for errors;
/// after an apply, 1 when any change failed or the edge did not take the
/// new snapshot.
fn exit_code(report: &pb::ConfigReport, planning: bool) -> i32 {
    if !report.errors.is_empty() || !report.snapshot_error.is_empty() {
        return 1;
    }

    if report.applied {
        let failed = report
            .changes
            .iter()
            .any(|change| change.outcome.starts_with("failed"));
        return if failed { 1 } else { 0 };
    }

    if planning && !report.changes.is_empty() {
        return 2;
    }

    0
}

/// The time now, as Unix milliseconds.
pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Unix milliseconds as an RFC 3339 instant in UTC.
pub(crate) fn format_time(unix_ms: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp((unix_ms / 1000) as i64)
        .map(|time| {
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                time.year(),
                time.month() as u8,
                time.day(),
                time.hour(),
                time.minute(),
                time.second()
            )
        })
        .unwrap_or_default()
}

/// Show or follow the events.
pub async fn events(
    follow: bool,
    after: u64,
    socket: SocketArgs,
    json: bool,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;

    if !follow {
        let answer = client
            .list_events(pb::ListEventsRequest { after, limit: 200 })
            .await
            .map_err(explain)?
            .into_inner();

        if json {
            return print_answer(&answer);
        }

        for event in answer.events {
            println!("{}", format_event(&event));
        }

        return Ok(());
    }

    // Followed, each event is a line of its own: JSON lines with --json.

    let mut stream = client
        .watch_events(pb::WatchEventsRequest { after })
        .await
        .map_err(explain)?
        .into_inner();

    while let Some(event) = stream.next().await {
        let event = event?;

        if json {
            println!("{}", serde_json::to_string(&event)?);
        } else {
            println!("{}", format_event(&event));
        }
    }

    Ok(())
}

/// What `muructl exec` takes: a release, or the project here, and the
/// command to run with it.
#[derive(Args)]
pub struct RunExecArgs {
    /// The application, or app@release; the current release when only
    /// the application is named. With none, the project in the current
    /// directory: through a muructl run or muructl serve of it when one
    /// is up, else by this command itself.
    #[arg(value_name = "APP[@RELEASE]")]
    target: Option<String>,

    /// The command and its arguments, after `--`, run without a shell;
    /// `python` is the release's interpreter.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    command: Vec<String>,

    /// Print each line of output, and how the command ended, as JSON
    /// lines of the admin API's command output.
    #[arg(long)]
    json: bool,

    /// Start the run and print its id, without waiting: it runs on under
    /// the daemon, its output in its own file and the release's log.
    #[arg(long, requires = "target")]
    detach: bool,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    project: crate::project_commands::ProjectArgs,
}

/// Run a one-off command with a release, or for the project here, and
/// exit with its status.
pub async fn exec(args: RunExecArgs) -> anyhow::Result<i32> {
    let place = Place::find(args.target.as_deref(), &args.socket, &args.project)?;
    let (app, release) = place.target()?;
    let request = pb::RunCommandRequest {
        app,
        release,
        argv: args.command,
        detach: args.detach,
        ..Default::default()
    };

    run_command(request, &place, args.json).await
}

/// Where a command runs (section 9.12): through an admin socket, the
/// daemon's for a named application and a development run's or muructl
/// serve's for the project here, or by this command itself.
enum Place<'a> {
    /// Through an admin socket, with the application and release named.
    Socket {
        /// The instance's admin endpoint.
        socket: client::Endpoint,
        /// The application and release, as given.
        target: Option<&'a str>,
    },
    /// By this command, for the project the flags find.
    Own(&'a crate::project_commands::ProjectArgs),
}

impl<'a> Place<'a> {
    /// With an application named, the daemon; with none, the socket
    /// given, else a muructl serve or development run of the project
    /// that answers, else this command itself.
    fn find(
        target: Option<&'a str>,
        socket: &SocketArgs,
        project: &'a crate::project_commands::ProjectArgs,
    ) -> anyhow::Result<Self> {
        if let Some(target) = target {
            if let Some(flag) = project.given() {
                anyhow::bail!(
                    "{flag} finds the project a command runs for when no application is \
                     named; {target} runs with its release's own code, under the daemon"
                );
            }

            return Ok(Self::Socket {
                socket: socket.endpoint()?,
                target: Some(target),
            });
        }

        if let Some(given) = socket.given_socket() {
            return Ok(Self::Socket {
                socket: client::Endpoint::unix(given),
                target: None,
            });
        }

        Ok(
            match socket.serving(&project.project(), project.chdir.is_some()) {
                Some(socket) => Self::Socket {
                    socket: client::Endpoint::unix(socket),
                    target: None,
                },
                None => Self::Own(project),
            },
        )
    }

    /// The application and release to ask for: those named, or none,
    /// which a run takes as the application it serves.
    fn target(&self) -> anyhow::Result<(String, String)> {
        match self {
            Self::Socket {
                target: Some(target),
                ..
            } => target_release(target),
            _ => Ok((String::new(), String::new())),
        }
    }

    /// The commands declared there.
    async fn commands(&self) -> anyhow::Result<pb::ListCommandsResponse> {
        match self {
            Self::Socket { socket, .. } => {
                let (app, release) = self.target()?;
                let mut client = client::connect(socket).await?;
                Ok(client
                    .list_commands(pb::ListCommandsRequest { app, release })
                    .await
                    .map_err(explain)?
                    .into_inner())
            }
            Self::Own(project) => {
                crate::project_commands::listed(&project.project(), project.file().as_deref())
                    .map_err(anyhow::Error::msg)
            }
        }
    }
}

/// The `muructl command` subcommands.
#[derive(Subcommand)]
pub enum CommandCommand {
    /// List the commands a release declares, with their inputs and each
    /// one's last run; or, with no application, those of the project in
    /// the current directory.
    List(Box<CommandListArgs>),

    /// Run a declared command, with its inputs, and exit with its status:
    /// a release's under the daemon, or, with no application, one of the
    /// project in the current directory.
    #[command(allow_missing_positional = true)]
    Run(Box<CommandRunArgs>),

    /// List the runs the daemon keeps of an application's commands, or of
    /// one command: those under way first, then the newest.
    Runs(Box<CommandRunsArgs>),

    /// Show one run: what started it, and how it ended or that it is
    /// still under way.
    Status(Box<RunIdArgs>),

    /// Show a run's output, from its own file, or follow it until the run
    /// ends.
    Logs(Box<CommandLogsArgs>),

    /// Stop a run under way, as at its timeout: SIGTERM to its session,
    /// and SIGKILL ten seconds later.
    Stop(Box<RunIdArgs>),
}

/// What `muructl command runs` takes.
#[derive(Args)]
pub struct CommandRunsArgs {
    /// The application.
    app: String,

    /// Only this command's runs; `exec` for one-off commands.
    name: Option<String>,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// What `muructl command status` and `muructl command stop` take.
#[derive(Args)]
pub struct RunIdArgs {
    /// The run's id, as `muructl command runs` lists it.
    run: u64,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// What `muructl command logs` takes.
#[derive(Args)]
pub struct CommandLogsArgs {
    /// The run's id, as `muructl command runs` lists it.
    run: u64,

    /// Keep printing lines as they are written, until the run ends.
    #[arg(long, short)]
    follow: bool,

    /// How many recent lines to show first.
    #[arg(long, short = 'n', default_value = "100", value_name = "COUNT")]
    lines: u32,

    /// Leave the time off each line.
    #[arg(long)]
    no_timestamps: bool,

    /// Print each line as JSON, as the admin API's log lines.
    #[arg(long)]
    json: bool,

    #[command(flatten)]
    socket: SocketArgs,
}

/// List the runs of an application's commands.
pub async fn command_runs(args: CommandRunsArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let answer = client
        .list_command_runs(pb::ListCommandRunsRequest {
            app: args.app.clone(),
            name: args.name.clone().unwrap_or_default(),
        })
        .await
        .map_err(explain)?
        .into_inner();

    if args.output.json {
        return print_answer(&answer);
    }

    if answer.runs.is_empty() {
        println!(
            "no runs of {} are kept",
            args.name.as_deref().unwrap_or("its commands")
        );
        return Ok(());
    }

    let width = answer
        .runs
        .iter()
        .map(|run| run.name.len())
        .max()
        .unwrap_or(0)
        .max("COMMAND".len());
    println!(
        "{:<6} {:<width$} {:<8} {:<20} {:<20} STATE",
        "RUN", "COMMAND", "RELEASE", "TRIGGER", "STARTED"
    );

    for run in &answer.runs {
        println!(
            "{:<6} {:<width$} {:<8} {:<20} {:<20} {}",
            run.run,
            run.name,
            run.release,
            run.trigger,
            format_time(run.started_unix_ms),
            run_state(run)
        );
    }

    Ok(())
}

/// A run's state, as a phrase.
fn run_state(run: &pb::CommandRunRecord) -> String {
    if !run.under_way {
        return run.message.clone();
    }

    let mut notes = Vec::new();

    if run.detached {
        notes.push("detached".to_owned());
    }

    if run.pid != 0 {
        notes.push(format!("pid {}", run.pid));
    }

    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", notes.join(", "))
    };

    if run.stopping.is_empty() {
        format!("under way{notes}")
    } else {
        format!("stopping because {}{notes}", run.stopping)
    }
}

/// Show one run.
pub async fn command_status(args: RunIdArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let run = client
        .get_command_run(pb::GetCommandRunRequest { run: args.run })
        .await
        .map_err(explain)?
        .into_inner();

    if args.output.json {
        return print_answer(&run);
    }

    print_run(&run);
    Ok(())
}

/// A run, a field a line.
fn print_run(run: &pb::CommandRunRecord) {
    println!(
        "run {} of {} for {}@{}",
        run.run, run.name, run.app, run.release
    );
    println!(
        "  started    {} by {}, {}{}",
        format_time(run.started_unix_ms),
        run.requested_by,
        run.trigger,
        if run.detached { ", detached" } else { "" }
    );

    if !run.inputs.is_empty() {
        println!("  inputs     {}", run.inputs);
    }

    if run.under_way {
        println!("  state      {}", run_state(run));
    } else {
        println!(
            "  ended      {}: it {}",
            format_time(run.ended_unix_ms),
            run.message
        );
        println!(
            "  exit       {}{}",
            run.exit_code,
            if run.ok { "" } else { ", failed" }
        );
    }

    println!("  output     {}", run.log);
}

/// Show or follow a run's output.
pub async fn command_logs(args: CommandLogsArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let mut lines = client
        .tail_command_run(pb::TailCommandRunRequest {
            run: args.run,
            lines: args.lines,
            follow: args.follow,
        })
        .await
        .map_err(explain)?
        .into_inner();

    while let Some(line) = lines.next().await {
        let line = line.map_err(explain)?;

        if args.json {
            println!("{}", serde_json::to_string(&line)?);
            continue;
        }

        let line = line.line;

        // Each line starts with the full UTC time, then a space.

        let shown = if args.no_timestamps {
            line.split_once(' ').map_or(line.as_str(), |(_, rest)| rest)
        } else {
            line.as_str()
        };
        println!("{shown}");
    }

    Ok(())
}

/// Stop a run.
pub async fn command_stop(args: RunIdArgs) -> anyhow::Result<()> {
    let mut client = client::connect(&args.socket.endpoint()?).await?;
    let run = client
        .stop_command_run(pb::StopCommandRunRequest { run: args.run })
        .await
        .map_err(explain)?
        .into_inner();

    if args.output.json {
        return print_answer(&run);
    }

    println!("muru: stopping run {} of {}", run.run, run.name);
    Ok(())
}

/// What `muructl command list` takes.
#[derive(Args)]
pub struct CommandListArgs {
    /// The application, or app@release; the current release when only
    /// the application is named. With none, the project here.
    #[arg(value_name = "APP[@RELEASE]")]
    target: Option<String>,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    project: crate::project_commands::ProjectArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// What `muructl command run` takes.
#[derive(Args)]
pub struct CommandRunArgs {
    /// The application, or app@release; the current release when only
    /// the application is named. With none, the project in the current
    /// directory: through a muructl run or muructl serve of it when one
    /// is up, else by this command itself.
    #[arg(value_name = "APP[@RELEASE]")]
    target: Option<String>,

    /// The command's name.
    name: String,

    /// An input's value.
    #[arg(long = "input", value_name = "NAME=VALUE", value_parser = parse_input)]
    inputs: Vec<(String, String)>,

    /// An input's value from one of the daemon's secret stores, so it
    /// never passes through this command; for a named application only.
    #[arg(long = "secret-input", value_name = "NAME=STORE/KEY", value_parser = parse_input)]
    secret_inputs: Vec<(String, String)>,

    /// An input whose value is read from standard input.
    #[arg(long, value_name = "NAME")]
    input_stdin: Option<String>,

    /// Print each line of output, and how the command ended, as JSON
    /// lines of the admin API's command output.
    #[arg(long)]
    json: bool,

    /// Start the run and print its id, without waiting: it runs on under
    /// the daemon, its output in its own file and the release's log.
    #[arg(long, requires = "target")]
    detach: bool,

    #[command(flatten)]
    socket: SocketArgs,

    #[command(flatten)]
    project: crate::project_commands::ProjectArgs,
}

/// Parse NAME=VALUE for --input and --secret-input.
fn parse_input(text: &str) -> Result<(String, String), String> {
    let (name, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not NAME=VALUE"))?;

    if !muru_controller::model::valid_env_name(name) {
        return Err(format!("{name:?} is not an input name"));
    }

    Ok((name.to_owned(), value.to_owned()))
}

/// List the commands a release declares, or the project here does.
pub async fn command_list(args: CommandListArgs) -> anyhow::Result<()> {
    let project = &args.project;
    let place = Place::find(args.target.as_deref(), &args.socket, project)?;
    let listed = place.commands().await?;

    if args.output.json {
        return print_answer(&listed);
    }

    if listed.release.is_empty() {
        println!(
            "{}, the project in {}",
            listed.app,
            project.project().display()
        );
    } else {
        println!("{}@{}", listed.app, listed.release);
    }

    if listed.commands.is_empty() {
        println!("  declares no commands");
    }

    for command in &listed.commands {
        println!();

        if command.description.is_empty() {
            println!("{}", command.name);
        } else {
            println!("{}: {}", command.name, command.description);
        }

        if command.call.is_empty() {
            println!("  run: {}", command.run.join(" "));
        } else {
            println!("  call: {}", command.call);
        }

        for input in &command.inputs {
            let mut notes = Vec::new();

            if input.required {
                notes.push("required".to_owned());
            }

            if input.secret {
                notes.push("secret".to_owned());
            }

            if !input.default.is_empty() {
                notes.push(format!("default {}", input.default));
            }

            if !input.pattern.is_empty() {
                notes.push(format!("matching {}", input.pattern));
            }

            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", notes.join(", "))
            };

            let description = if input.description.is_empty() {
                String::new()
            } else {
                format!(": {}", input.description)
            };
            println!("  input {}{notes}{description}", input.name);
        }

        let mut limits = Vec::new();

        if !command.timeout.is_empty() {
            limits.push(format!("timeout {}", command.timeout));
        }

        if command.exclusive {
            limits.push("one run at a time".to_owned());
        }

        if !limits.is_empty() {
            println!("  {}", limits.join(", "));
        }

        if !command.gates.is_empty() {
            println!(
                "  gate: {}{}",
                command.gates.join(", "),
                if command.every_start {
                    ", at every start of the host"
                } else {
                    ", once a release"
                }
            );
        }

        if !command.schedule.is_empty() {
            let next = if command.next_run_unix_ms == 0 {
                String::new()
            } else {
                format!("; next {}", format_time(command.next_run_unix_ms))
            };
            println!("  schedule: {}{next}", command.schedule);
        }

        if let Some(run) = &command.last_run {
            println!(
                "  last run: {} with {}, {}",
                format_time(run.started_unix_ms),
                run.release,
                run.message
            );
        }
    }

    Ok(())
}

/// Run a declared command and exit with its status. A required input
/// not given is asked for when standard input is a terminal, a secret one
/// without echo; otherwise the run is refused, saying which.
pub async fn command_run(args: CommandRunArgs) -> anyhow::Result<i32> {
    use std::io::{IsTerminal as _, Read as _};

    let place = Place::find(args.target.as_deref(), &args.socket, &args.project)?;
    let (app, release) = place.target()?;
    let mut inputs: std::collections::BTreeMap<String, String> = args.inputs.into_iter().collect();
    let secret_inputs: std::collections::BTreeMap<String, String> =
        args.secret_inputs.into_iter().collect();

    if let Some(name) = &args.input_stdin {
        let mut value = String::new();
        std::io::stdin()
            .read_to_string(&mut value)
            .context("reading the input from standard input")?;
        inputs.insert(
            name.clone(),
            value.trim_end_matches(['\n', '\r']).to_owned(),
        );
    } else if std::io::stdin().is_terminal() && !args.json {
        let listed = place.commands().await?;
        let declared = listed
            .commands
            .iter()
            .find(|command| command.name == args.name);

        for input in declared
            .map(|command| &command.inputs[..])
            .unwrap_or_default()
        {
            if input.required
                && !inputs.contains_key(&input.name)
                && !secret_inputs.contains_key(&input.name)
            {
                let value = prompt(&input.name, &input.description, input.secret)?;
                inputs.insert(input.name.clone(), value);
            }
        }
    }

    let request = pb::RunCommandRequest {
        app,
        release,
        name: args.name,
        inputs,
        secret_inputs,
        detach: args.detach,
        ..Default::default()
    };

    run_command(request, &place, args.json).await
}

/// Ask for an input's value on the terminal, without echo for a secret
/// one.
fn prompt(name: &str, description: &str, secret: bool) -> anyhow::Result<String> {
    use std::io::Write as _;

    let label = if description.is_empty() {
        name.to_owned()
    } else {
        format!("{name} ({description})")
    };
    eprint!("{label}: ");
    std::io::stderr().flush()?;

    // stty acts on the terminal standard input is, which it inherits.

    let echo = |on: bool| {
        std::process::Command::new("stty")
            .arg(if on { "echo" } else { "-echo" })
            .status()
    };

    if secret {
        echo(false).context("turning off the terminal's echo")?;
    }

    let mut value = String::new();
    let read = std::io::stdin().read_line(&mut value);

    if secret {
        let _ = echo(true);
        eprintln!();
    }

    read.context("reading the input")?;
    Ok(value.trim_end_matches(['\n', '\r']).to_owned())
}

/// The application and release a target names: app@release, or the
/// application alone for its current release.
fn target_release(target: &str) -> anyhow::Result<(String, String)> {
    if target.contains('@') {
        let (app, release) = split_release(target)?;
        Ok((app.to_owned(), release.to_owned()))
    } else {
        Ok((target.to_owned(), String::new()))
    }
}

/// Run a command where it belongs, printing its output as it arrives,
/// standard error to standard error, and return its exit status. A
/// command that fails says how on standard error, as muructl's other
/// errors do.
async fn run_command(
    request: pb::RunCommandRequest,
    place: &Place<'_>,
    json: bool,
) -> anyhow::Result<i32> {
    let socket = match place {
        Place::Socket { socket, .. } => socket,
        Place::Own(project) => return run_own(request, project, json).await,
    };

    let mut client = client::connect(socket).await?;
    let mut stream = client
        .run_command(request)
        .await
        .map_err(explain)?
        .into_inner();

    while let Some(output) = stream.next().await {
        if let Some(code) = show_output(&output.map_err(explain)?, json)? {
            return Ok(code);
        }
    }

    anyhow::bail!("the daemon stopped answering before the command ended")
}

/// Print a message of a command's stream, and give muructl's exit status
/// when it is the last. Output that cannot be written, to a terminal that
/// has gone or a pipe nothing reads, is an error rather than a panic.
fn show_output(output: &pb::CommandOutput, json: bool) -> anyhow::Result<Option<i32>> {
    use std::io::Write as _;

    let mut stdout = std::io::stdout().lock();

    if json {
        writeln!(stdout, "{}", serde_json::to_string(output)?)?;
    } else if output.detached {
        writeln!(
            stdout,
            "muru: started run {}; muructl command logs {} --follow follows it",
            output.run, output.run
        )?;
    } else if output.finished {
        if !output.ok {
            writeln!(std::io::stderr(), "muru: the command {}", output.message)?;
        }
    } else if output.stderr {
        writeln!(std::io::stderr(), "{}", output.line)?;
    } else {
        writeln!(stdout, "{}", output.line)?;
    }

    if output.finished {
        stdout.flush()?;
        return Ok(Some(exit_status(output)));
    }

    if output.detached {
        stdout.flush()?;
        return Ok(Some(0));
    }

    Ok(None)
}

/// muructl's exit status for how a command ended: the command's own, or
/// 1 for a command that was stopped and exited with 0 all the same.
fn exit_status(output: &pb::CommandOutput) -> i32 {
    if !output.ok && output.exit_code == 0 {
        1
    } else {
        output.exit_code
    }
}

/// Run a command of the project here in this process (section 9.12):
/// nothing else is running to ask, as in a Kubernetes Job. The command
/// gets what muructl serve and muructl run would serve the application
/// with. SIGINT, SIGTERM or SIGHUP stops it as a caller going away does
/// under the daemon, and so does output muructl can no longer write; a
/// second signal kills it at once.
async fn run_own(
    request: pb::RunCommandRequest,
    project: &crate::project_commands::ProjectArgs,
    json: bool,
) -> anyhow::Result<i32> {
    use tokio::signal::unix::{SignalKind, signal};

    let request = muru_controller::commands::CommandRequest::from_message(request)
        .map_err(anyhow::Error::msg)?;
    let context = crate::project_commands::own(project, &request).await?;
    let prepared =
        crate::project_commands::prepare(&context, request).map_err(anyhow::Error::msg)?;

    // The signals are taken before the command starts, so none that comes
    // once it has is missed.

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let (signalled, signals) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(async move {
        loop {
            let signal = tokio::select! {
                _ = interrupt.recv() => libc::SIGINT,
                _ = terminate.recv() => libc::SIGTERM,
                _ = hangup.recv() => libc::SIGHUP,
            };

            if signalled.send(signal).is_err() {
                return;
            }
        }
    });

    let running = muru_controller::commands::run(
        prepared,
        std::sync::Arc::new(muru_controller::commands::Unrecorded),
    )
    .map_err(anyhow::Error::msg)?;

    relay(running, signals, |message| show_output(message, json)).await
}

/// What ended relaying a command's output before the command ended.
enum Interrupted {
    /// muructl got this signal.
    Signal(i32),
    /// Its output could not be written.
    Unwritable(anyhow::Error),
}

/// Relay a running command's output with `show` and give muructl's exit
/// status. A signal, or output `show` cannot write, stops the command as
/// at its timeout; the output is read on to the last message, which comes
/// once the command has ended and been reaped, so muructl exits only after
/// it. Another signal kills the command's session at once.
async fn relay(
    running: muru_controller::commands::Running,
    mut signals: tokio::sync::mpsc::UnboundedReceiver<i32>,
    mut show: impl FnMut(&pb::CommandOutput) -> anyhow::Result<Option<i32>>,
) -> anyhow::Result<i32> {
    let muru_controller::commands::Running {
        mut output,
        stopper,
    } = running;

    let interrupted = loop {
        tokio::select! {
            message = output.recv() => {
                let Some(message) = message else {
                    anyhow::bail!("the command's end was not reported");
                };

                match show(&message) {
                    Ok(Some(code)) => return Ok(code),
                    Ok(None) => {}
                    Err(error) => {
                        break Interrupted::Unwritable(
                            error.context("writing the command's output"),
                        );
                    }
                }
            }
            Some(signal) = signals.recv() => break Interrupted::Signal(signal),
        }
    };

    let why = match interrupted {
        Interrupted::Signal(libc::SIGINT) => "muructl got SIGINT",
        Interrupted::Signal(libc::SIGHUP) => "muructl got SIGHUP",
        Interrupted::Signal(_) => "muructl got SIGTERM",
        Interrupted::Unwritable(_) => "muructl could not write its output",
    };

    stopper.stop(why);

    loop {
        tokio::select! {
            message = output.recv() => {
                let Some(message) = message else {
                    anyhow::bail!("the command's end was not reported");
                };

                // After a signal the output is shown while it can be: after
                // SIGHUP the terminal may be gone, and that does not stop
                // muructl waiting for the command. Output that could not be
                // written is not tried again.

                if matches!(interrupted, Interrupted::Signal(_)) {
                    let _ = show(&message);
                }

                if message.finished {
                    return match interrupted {
                        Interrupted::Signal(signal) => Ok(128 + signal),
                        Interrupted::Unwritable(error) => Err(error),
                    };
                }
            }
            Some(_) = signals.recv() => stopper.kill(),
        }
    }
}

/// What `muructl logs` takes: a release's output, the access log, or
/// the edge's or the daemon's own log, shown or followed; or a
/// subcommand on the log files.
#[derive(Args, Clone)]
#[command(args_conflicts_with_subcommands = true)]
pub struct LogsArgs {
    #[command(subcommand)]
    pub command: Option<LogsCommand>,

    /// The application, or app@release; the current release when only
    /// the application is named. A development run's application when
    /// left out; the daemon needs it, unless --access, --edge or
    /// --daemon is given.
    #[arg(value_name = "APP[@RELEASE]", conflicts_with_all = ["edge", "daemon"])]
    target: Option<String>,

    /// Keep printing lines as they are written.
    #[arg(long, short)]
    follow: bool,

    /// How many recent lines to show first.
    #[arg(long, short = 'n', default_value = "100", value_name = "COUNT")]
    lines: u32,

    /// The access log the edge writes, instead of a release's output.
    #[arg(long, conflicts_with_all = ["edge", "daemon"])]
    access: bool,

    /// The daemon's edge process's own output, edge.log.
    #[arg(long, conflicts_with = "daemon")]
    edge: bool,

    /// The daemon's own output, daemon.log, which a daemon started with
    /// muructl daemon start writes.
    #[arg(long)]
    daemon: bool,

    /// With --access, the records of one virtual host: from the file it
    /// names for them, or else those in the access log that name it.
    #[arg(long, value_name = "HOST", requires = "access")]
    host: Option<String>,

    /// Print each line as JSON: a release's output as the admin API's
    /// log lines, access log records as the JSON lines they are kept as.
    #[arg(long)]
    json: bool,

    /// Leave off the UTC time each line starts with.
    #[arg(long)]
    no_timestamps: bool,

    #[command(flatten)]
    socket: SocketArgs,

    /// With no daemon running, where its log directory and state are:
    /// the files are read from there.
    #[command(flatten)]
    files: DaemonFileArgs,
}

/// Which logs a command on the log files acts on.
#[derive(Args, Clone, Default)]
pub struct LogTargetArgs {
    /// One release's log, as app@release.
    #[arg(value_name = "APP@RELEASE")]
    target: Option<String>,

    /// The access log.
    #[arg(long)]
    access: bool,

    /// The file a virtual host names for its records.
    #[arg(long, value_name = "HOST")]
    host: Option<String>,

    /// Every log.
    #[arg(long)]
    all: bool,
}

impl LogTargetArgs {
    /// The target as the admin API takes it. `required` refuses none
    /// being named, saying what `doing` would need.
    fn target(&self, required: bool, doing: &str) -> anyhow::Result<pb::LogTarget> {
        let named = usize::from(self.target.is_some())
            + usize::from(self.access)
            + usize::from(self.host.is_some())
            + usize::from(self.all);

        if named > 1 {
            anyhow::bail!("name one of APP@RELEASE, --access, --host HOST or --all, not several");
        }

        if named == 0 && required {
            anyhow::bail!("name what to {doing}: APP@RELEASE, --access, --host HOST or --all");
        }

        let (app, release) = match &self.target {
            Some(target) => {
                let (app, release) = split_release(target)?;
                (app.to_owned(), release.to_owned())
            }
            None => (String::new(), String::new()),
        };

        Ok(pb::LogTarget {
            app,
            release,
            access: self.access,
            host: self.host.clone().unwrap_or_default(),
            all: self.all,
        })
    }
}

/// What `muructl logs truncate` takes.
#[derive(Args, Clone)]
pub struct LogsTruncateArgs {
    #[command(flatten)]
    target: LogTargetArgs,

    /// How much of the newest content to keep, as a size such as 100MB,
    /// cut at a line boundary; 0 empties the file.
    #[arg(long, value_name = "SIZE", value_parser = muru_config::parse_size)]
    keep: u64,

    /// Do it without asking.
    #[arg(long)]
    yes: bool,

    #[command(flatten)]
    socket: SocketArgs,

    /// With no daemon running, where its log directory and state are.
    #[command(flatten)]
    files: DaemonFileArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// What `muructl logs rotate` takes.
#[derive(Args, Clone)]
pub struct LogsRotateArgs {
    #[command(flatten)]
    target: LogTargetArgs,

    #[command(flatten)]
    socket: SocketArgs,

    /// With no daemon running, where its log directory, state and
    /// configuration are.
    #[command(flatten)]
    files: DaemonFileArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// What `muructl logs prune` takes.
#[derive(Args, Clone)]
pub struct LogsPruneArgs {
    /// Keep only this many of the newest archives per log.
    #[arg(long, value_name = "N", conflicts_with = "older_than")]
    keep: Option<u32>,

    /// Remove archives older than this, as a duration such as 30d.
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    older_than: Option<Duration>,

    /// Also remove the logs and archives of releases and virtual hosts
    /// that no longer exist.
    #[arg(long)]
    orphaned: bool,

    /// Only list what would be removed.
    #[arg(long)]
    dry_run: bool,

    /// Do it without asking.
    #[arg(long)]
    yes: bool,

    #[command(flatten)]
    socket: SocketArgs,

    /// With no daemon running, where its log directory and state are.
    #[command(flatten)]
    files: DaemonFileArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// The `muructl logs` subcommands.
#[derive(Subcommand, Clone)]
pub enum LogsCommand {
    /// Reopen every log file by name, after logrotate has renamed them.
    Reopen {
        /// Also run on_reopen_logs in every worker, for files the
        /// application opened itself.
        #[arg(long)]
        workers: bool,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Remove log archives by count or age and, with --orphaned, the
    /// logs and archives of releases and hosts that no longer exist.
    /// Asks first; without a terminal it needs --yes.
    Prune(Box<LogsPruneArgs>),

    /// Rotate logs now, on the policy in force: all of them when none
    /// is named, compressing and keeping as configured.
    Rotate(Box<LogsRotateArgs>),

    /// Cut a live log down to its newest SIZE, at a line boundary. Asks
    /// first; without a terminal it needs --yes.
    Truncate(Box<LogsTruncateArgs>),

    /// List each log with its size, its archives and the total, and flag
    /// logs whose release or host no longer exists.
    List(Box<LogsListArgs>),
}

/// What `muructl logs list` takes.
#[derive(Args, Clone)]
pub struct LogsListArgs {
    /// Only the logs of this application's releases, or of one release
    /// as app@release.
    #[arg(value_name = "APP[@RELEASE]", conflicts_with_all = ["access", "host"])]
    target: Option<String>,

    /// Only the access log.
    #[arg(long, conflicts_with = "host")]
    access: bool,

    /// Only the file a virtual host names for its records.
    #[arg(long, value_name = "HOST")]
    host: Option<String>,

    /// List each archive under its log, oldest first, with its size and
    /// when it was last written.
    #[arg(long)]
    archives: bool,

    #[command(flatten)]
    socket: SocketArgs,

    /// With no daemon running, where its log directory and state are.
    #[command(flatten)]
    files: DaemonFileArgs,

    #[command(flatten)]
    output: JsonArgs,
}

impl LogsListArgs {
    /// Whether the arguments name a log: every log when they name none.
    fn names(&self, log: &pb::LogInfo) -> bool {
        if let Some(target) = &self.target {
            let (app, release) = target.split_once('@').unwrap_or((target, ""));

            // An application alone lists its commands' runs too, which
            // belong to no one release.

            return match log.kind.as_str() {
                "release" => log.app == app && (release.is_empty() || log.release == release),
                "commands" => log.app == app && release.is_empty(),
                _ => false,
            };
        }

        if let Some(host) = &self.host {
            return log.hosts.contains(host);
        }

        !self.access || log.kind == "access"
    }

    /// Keep only the logs the arguments name, with the total of those
    /// alone. Naming a log that is not there is refused.
    fn narrow(&self, report: &mut pb::ListLogsResponse) -> anyhow::Result<()> {
        let missing = match (&self.target, &self.host) {
            (Some(target), _) => {
                if target.contains('@') {
                    split_release(target)?;
                }

                format!("no log for {target}")
            }
            (None, Some(host)) => format!(
                "no log file for virtual host {host}: its records are in access.log unless it \
                 names an access_log_file"
            ),
            (None, None) if self.access => "there is no access log".to_owned(),
            (None, None) => return Ok(()),
        };

        report.logs.retain(|log| self.names(log));

        if report.logs.is_empty() {
            anyhow::bail!("{missing}, in {}", report.directory);
        }

        report.total = report.logs.iter().map(|log| log.total).sum();
        Ok(())
    }
}

/// The lines `muructl logs` prints, from the daemon or from a file.
type LogLines = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = anyhow::Result<pb::LogLine>>>>;

/// Show or follow a release's output, the access log, or the edge's or
/// the daemon's own log: through whatever answers at the admin socket,
/// else, with no daemon running, from the file in the log directory.
pub async fn logs(args: LogsArgs) -> anyhow::Result<()> {
    let (mut lines, named) = if args.edge || args.daemon {
        (own_log_lines(&args).await?, None)
    } else {
        log_lines(&args).await?
    };

    // Access log records are kept as JSON lines already; any other line
    // becomes a JSON line of the admin API's log line. `named` is a host
    // whose records are picked out of access.log by its name.

    while let Some(line) = lines.next().await {
        let line = line?;

        if let Some(name) = &named {
            let of_host = serde_json::from_str::<serde_json::Value>(&line.line)
                .ok()
                .and_then(|record| Some(record.get("virtual_host")?.as_str()? == name))
                .unwrap_or(false);

            if !of_host {
                continue;
            }
        }

        match (args.access, args.json) {
            (true, false) => println!("{}", format_access_line(&line.line, !args.no_timestamps)),
            (false, true) => println!("{}", serde_json::to_string(&line)?),
            (true, true) => println!("{}", line.line),
            (false, false) if args.no_timestamps => {
                println!("{}", muru_common::log::split_time(&line.line).1);
            }
            (false, false) => println!("{}", line.line),
        }
    }

    Ok(())
}

/// Whether a virtual host's records are in a file of its own, which it
/// names, rather than among access.log's. A host whose records go to
/// access.log in the combined format cannot be picked out of it.
fn host_has_own_file(name: &str, access_log_file: &str, format: &str) -> anyhow::Result<bool> {
    let own_file = !access_log_file.is_empty();

    if !own_file && format == "combined" {
        anyhow::bail!(
            "host {name} writes its records to access.log in the combined format, which does \
             not name the host; give the host an access_log_file to keep them apart"
        );
    }

    Ok(own_file)
}

/// A release's output or the access log, with the host whose records are
/// to be picked out of it. With no daemon listening and no socket named,
/// they come from the files.
async fn log_lines(args: &LogsArgs) -> anyhow::Result<(LogLines, Option<String>)> {
    let endpoint = args.socket.endpoint()?;
    let locations = daemon_locations(&args.socket, &args.files);

    if let Some(path) = endpoint.socket()
        && path == locations.admin_socket()
        && no_daemon(&args.socket, path)
    {
        return file_log_lines(args, &locations);
    }

    let (app, release) = match &args.target {
        Some(target) if target.contains('@') => {
            let (app, release) = split_release(target)?;
            (app.to_owned(), release.to_owned())
        }
        Some(app) => (app.clone(), String::new()),
        None => (String::new(), String::new()),
    };

    let mut client = client::connect(&endpoint).await?;

    // Section 11.2: a virtual host's records are in the file it names
    // for them, or else among the access log's, where each JSON record
    // names its virtual host.

    let mut own_file = false;

    if let Some(name) = &args.host {
        let hosts = client
            .list_hosts(pb::ListHostsRequest {})
            .await
            .map_err(explain)?
            .into_inner()
            .hosts;
        let host = hosts
            .iter()
            .find(|host| host.name == *name)
            .ok_or_else(|| anyhow::anyhow!("no virtual host named {name}"))?;
        own_file = host_has_own_file(name, &host.access_log_file, &host.access_log_format)?;
    }

    let stream = client
        .tail_logs(pb::TailLogsRequest {
            app,
            release,
            follow: args.follow,
            lines: args.lines,
            access: args.access,
            host: args.host.clone().filter(|_| own_file).unwrap_or_default(),
            ..pb::TailLogsRequest::default()
        })
        .await
        .map_err(explain)?
        .into_inner();

    let named = args.host.clone().filter(|_| !own_file);
    Ok((Box::pin(stream.map(|line| line.map_err(explain))), named))
}

/// A release's output or the access log read from its file, with no
/// daemon running. A release named in full and the access log need only
/// the log directory; an application's current release and a virtual
/// host's file are found in the state store, opened read-only.
fn file_log_lines(
    args: &LogsArgs,
    locations: &Locations,
) -> anyhow::Result<(LogLines, Option<String>)> {
    let store = |needed_for: &str| -> anyhow::Result<muru_controller::store::Store> {
        let path = locations.state_store();

        if !path.is_file() {
            anyhow::bail!(
                "no daemon is running, and {needed_for} needs its state store, which is not at \
                 {}; name it with --state-dir",
                path.display()
            );
        }

        muru_controller::store::Store::open_read_only(&path)
            .with_context(|| format!("opening {}", path.display()))
    };

    let (app, release, path, named) = if args.access {
        match &args.host {
            None => (String::new(), String::new(), locations.access_log(), None),
            Some(name) => {
                let hosts = store("finding a virtual host's records")?
                    .hosts()
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                let (_, host) = hosts
                    .iter()
                    .find(|(host, _)| host == name)
                    .ok_or_else(|| anyhow::anyhow!("no virtual host named {name}"))?;
                let file = host.access_log_file.clone().unwrap_or_default();
                let format = host.access_log_format.clone().unwrap_or_default();

                if host_has_own_file(name, &file, &format)? {
                    (
                        String::new(),
                        String::new(),
                        locations.logs.path.join(file),
                        None,
                    )
                } else {
                    (
                        String::new(),
                        String::new(),
                        locations.access_log(),
                        Some(name.clone()),
                    )
                }
            }
        }
    } else {
        let Some(target) = &args.target else {
            anyhow::bail!("name an application, or give --access, --edge or --daemon");
        };

        let (app, release) = match target.split_once('@') {
            Some(_) => {
                let (app, release) = split_release(target)?;
                (app.to_owned(), release.to_owned())
            }
            None => {
                let found = store("finding an application's current release")?
                    .app(target)
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                let current = found
                    .current
                    .ok_or_else(|| anyhow::anyhow!("{target} has no current release"))?;
                (target.clone(), current)
            }
        };

        let path = locations.release_log(&app, &release);
        (app, release, path, None)
    };

    if !args.follow && !path.exists() {
        anyhow::bail!(muru_controller::logs::missing(&path));
    }

    let lines = muru_controller::logs::tail(path, args.lines as usize, args.follow);
    let stream = lines.map(move |line| {
        Ok(pb::LogLine {
            app: app.clone(),
            release: release.clone(),
            line,
        })
    });

    Ok((Box::pin(stream), named))
}

/// The edge's or the daemon's own log: through the daemon when one is
/// running, else from the file in the log directory, which is where a
/// daemon that failed to start says why.
async fn own_log_lines(args: &LogsArgs) -> anyhow::Result<LogLines> {
    match logs_way(&args.socket, &args.files).await? {
        LogsWay::Daemon(mut client) => {
            let stream = client
                .tail_logs(pb::TailLogsRequest {
                    follow: args.follow,
                    lines: args.lines,
                    edge: args.edge,
                    daemon: args.daemon,
                    ..pb::TailLogsRequest::default()
                })
                .await
                .map_err(explain)?
                .into_inner();

            Ok(Box::pin(stream.map(|line| line.map_err(explain))))
        }
        LogsWay::Files(directory) => {
            let path = if args.edge {
                directory.locations.edge_log()
            } else {
                directory.locations.daemon_log()
            };

            if !args.follow && !path.exists() {
                anyhow::bail!(muru_controller::logs::missing(&path));
            }

            let lines = muru_controller::logs::tail(path, args.lines as usize, args.follow);

            Ok(Box::pin(lines.map(|line| {
                Ok(pb::LogLine {
                    line,
                    ..pb::LogLine::default()
                })
            })))
        }
    }
}

/// Reopen every log file after an external rotation.
pub async fn logs_reopen(workers: bool, socket: SocketArgs, json: bool) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let reopened = client
        .reopen_logs(pb::ReopenLogsRequest { workers })
        .await
        .map_err(explain)?
        .into_inner();

    if json {
        return print_answer(&reopened);
    }

    // With no host to reopen, as a development run or a daemon with no
    // release running, the edge alone is named.

    if reopened.edge && reopened.hosts == 0 {
        println!("muru: reopened the log files: the edge");
        return Ok(());
    }

    println!(
        "muru: reopened the log files: {}{} host{}{}",
        if reopened.edge { "the edge and " } else { "" },
        reopened.hosts,
        if reopened.hosts == 1 { "" } else { "s" },
        if workers { " and their workers" } else { "" }
    );
    Ok(())
}

/// How a command on the log files reaches them: through the running
/// daemon, or with none running on the files themselves.
enum LogsWay {
    /// Through the daemon, so that it can tell the writers and nothing
    /// collides with its rotation.
    Daemon(Box<muru_proto::admin::v1::admin_client::AdminClient<tonic::transport::Channel>>),
    /// On the files directly, with none running.
    Files(Box<LogDirectory>),
}

/// A log directory with no daemon running.
struct LogDirectory {
    locations: Locations,
    /// What the state store says exists, when there is a store.
    existing: Option<muru_controller::log_files::Existing>,
}

/// The locations the daemon's location options resolve to.
pub(crate) fn daemon_locations(socket: &SocketArgs, files: &DaemonFileArgs) -> Locations {
    Locations::resolve(&Options {
        config: files.config.clone(),
        state_dir: files.state_dir.clone(),
        run_dir: socket.locations.run_dir.clone(),
        log_dir: files.log_dir.clone(),
        cache_dir: files.cache_dir.clone(),
        system: socket.locations.system,
        instance: socket.locations.instance(),
    })
}

/// Whether nothing listens where a command that names no socket would
/// find the daemon. A socket file that is absent, or one left behind that
/// refuses the connection, is no daemon. Anything else, such as a
/// permission refused, is left for the connection to report.
pub(crate) fn no_daemon(socket: &SocketArgs, path: &Path) -> bool {
    socket.given_socket().is_none()
        && match path.try_exists() {
            Ok(false) => true,
            Ok(true) => muru_controller::client::left_behind(path),
            Err(_) => false,
        }
}

/// Find how to reach the log files. A command that names no socket looks
/// for the daemon alone, since only a daemon writes log files; with none
/// listening it works on the files in the resolved log directory.
async fn logs_way(socket: &SocketArgs, files: &DaemonFileArgs) -> anyhow::Result<LogsWay> {
    let locations = daemon_locations(socket, files);
    require_local("the logs commands on files", locations.instance.as_deref())?;
    let path = socket
        .given_socket()
        .unwrap_or_else(|| locations.admin_socket());

    if no_daemon(socket, &path) {
        let store_path = locations.state_store();

        let existing = if store_path.is_file() {
            let store = muru_controller::store::Store::open_read_only(&store_path)
                .with_context(|| format!("opening {}", store_path.display()))?;
            Some(muru_controller::log_files::Existing::read(&store)?)
        } else {
            None
        };

        return Ok(LogsWay::Files(Box::new(LogDirectory {
            locations,
            existing,
        })));
    }

    let mut client = client::connect(&client::Endpoint::unix(&path)).await?;

    if let Some(asked) = &files.log_dir {
        let answer = client
            .list_logs(pb::ListLogsRequest {})
            .await
            .map_err(explain)?
            .into_inner();
        let same = asked.canonicalize().ok() == Path::new(&answer.directory).canonicalize().ok();

        if !same {
            anyhow::bail!(
                "the daemon running at {} writes its logs to {}, not {}",
                path.display(),
                answer.directory,
                asked.display()
            );
        }
    }

    Ok(LogsWay::Daemon(Box::new(client)))
}

/// List the log files and what they take.
pub async fn logs_list(args: LogsListArgs) -> anyhow::Result<()> {
    let mut report = match logs_way(&args.socket, &args.files).await? {
        LogsWay::Daemon(mut client) => client
            .list_logs(pb::ListLogsRequest {})
            .await
            .map_err(explain)?
            .into_inner(),
        LogsWay::Files(directory) => muru_controller::log_files::report(
            &directory.locations.logs.path,
            directory.existing.as_ref(),
        )
        .with_context(|| format!("reading {}", directory.locations.logs.path.display()))?,
    };

    args.narrow(&mut report)?;

    if args.output.json {
        return print_answer(&report);
    }

    if report.logs.is_empty() {
        println!("no log files in {}", report.directory);
        return Ok(());
    }

    if args.archives {
        print_archives(&report);
    } else {
        print_logs(&report);
    }

    println!(
        "total {}, in {}",
        human_size(report.total),
        report.directory
    );

    if !report.orphans_known {
        println!(
            "no state store was found, so logs of releases and hosts that no longer exist \
             are not flagged"
        );
    }

    Ok(())
}

/// The logs as a table: each with its size, how many archives it has
/// and what they take, and the total.
fn print_logs(report: &pb::ListLogsResponse) {
    println!(
        "{:<40} {:>9} {:>18} {:>9}",
        "LOG", "SIZE", "ARCHIVES", "TOTAL"
    );

    for log in &report.logs {
        // A command's runs' files are listed as its archives are.

        let archives = if log.kind == "commands" {
            format!(
                "{} run{} ({})",
                log.archives.len(),
                if log.archives.len() == 1 { "" } else { "s" },
                human_size(log.archives_size)
            )
        } else if log.archives.is_empty() {
            "-".to_owned()
        } else {
            format!("{} ({})", log.archives.len(), human_size(log.archives_size))
        };

        let size = if log.present {
            human_size(log.size)
        } else {
            "-".to_owned()
        };

        println!(
            "{:<40} {size:>9} {archives:>18} {:>9}{}",
            log.name,
            human_size(log.total),
            if log.orphaned { "  orphaned" } else { "" }
        );
    }
}

/// The logs as a table with each archive on a line of its own under its
/// log, oldest first: every file with its size and when it was last
/// written, and each log with its total.
fn print_archives(report: &pb::ListLogsResponse) {
    let width = report
        .logs
        .iter()
        .flat_map(|log| {
            std::iter::once(log.name.len())
                .chain(log.archives.iter().map(|archive| archive.name.len() + 2))
        })
        .max()
        .unwrap_or(0)
        .max(40);

    println!(
        "{:<width$} {:>9}  {:<20} {:>9}",
        "LOG", "SIZE", "MODIFIED", "TOTAL"
    );

    for log in &report.logs {
        let (size, modified) = if log.present {
            (human_size(log.size), format_time(log.modified_unix_ms))
        } else {
            ("-".to_owned(), "-".to_owned())
        };

        println!(
            "{:<width$} {size:>9}  {modified:<20} {:>9}{}",
            log.name,
            human_size(log.total),
            if log.orphaned { "  orphaned" } else { "" }
        );

        for archive in &log.archives {
            println!(
                "{:<width$} {:>9}  {}{}",
                format!("  {}", archive.name),
                human_size(archive.size),
                format_time(archive.modified_unix_ms),
                if archive.orphaned { "  orphaned" } else { "" }
            );
        }
    }
}

/// Ask a yes or no question on the terminal, on standard error so that
/// standard output stays the command's answer. `--yes` answers without
/// asking; with no terminal to ask on, the command is refused.
pub(crate) fn confirm(question: &str, yes: bool) -> anyhow::Result<bool> {
    use std::io::{IsTerminal as _, Write as _};

    if yes {
        return Ok(true);
    }

    if !std::io::stdin().is_terminal() {
        anyhow::bail!("there is no terminal to ask {question:?}; give --yes to go ahead");
    }

    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;

    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Cut live logs down to their newest content.
pub async fn logs_truncate(args: LogsTruncateArgs) -> anyhow::Result<()> {
    let target = args.target.target(true, "truncate")?;
    let keep = args.keep;
    let way = logs_way(&args.socket, &args.files).await?;

    // What it would free, for the question.

    let plan = match &way {
        LogsWay::Daemon(client) => client
            .clone()
            .truncate_logs(pb::TruncateLogsRequest {
                target: Some(target.clone()),
                keep,
                dry_run: true,
            })
            .await
            .map_err(explain)?
            .into_inner(),
        LogsWay::Files(directory) => muru_controller::log_files::plan_truncate(
            &directory.locations.logs.path,
            directory.existing.as_ref(),
            &target,
            keep,
        )
        .map_err(anyhow::Error::msg)?,
    };

    if plan.freed == 0 {
        eprintln!(
            "muru: nothing to truncate: every log named is within {}",
            human_size(keep)
        );
        return if args.output.json {
            print_answer(&plan)
        } else {
            Ok(())
        };
    }

    for log in &plan.logs {
        eprintln!(
            "  {} from {} to {}, freeing {}",
            log.name,
            human_size(log.size),
            human_size(log.kept),
            human_size(log.freed)
        );
    }

    let question = format!("Truncate, freeing {}?", human_size(plan.freed));

    if !confirm(&question, args.yes)? {
        eprintln!("muru: nothing was changed");
        return Ok(());
    }

    let answer = match way {
        LogsWay::Daemon(mut client) => client
            .truncate_logs(pb::TruncateLogsRequest {
                target: Some(target),
                keep,
                dry_run: false,
            })
            .await
            .map_err(explain)?
            .into_inner(),
        LogsWay::Files(directory) => muru_controller::log_files::truncate_directly(
            &directory.locations.logs.path,
            directory.existing.as_ref(),
            &target,
            keep,
        )
        .map_err(anyhow::Error::msg)?,
    };

    if args.output.json {
        print_answer(&answer)?;
    } else {
        println!("muru: truncated, freeing {}", human_size(answer.freed));
    }

    let failures: Vec<String> = answer
        .logs
        .iter()
        .filter(|log| !log.error.is_empty())
        .map(|log| format!("{}: {}", log.name, log.error))
        .collect();

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(failures.join("; "))
    }
}

/// Rotate logs now.
pub async fn logs_rotate(args: LogsRotateArgs) -> anyhow::Result<()> {
    let target = args.target.target(false, "rotate")?;

    let answer = match logs_way(&args.socket, &args.files).await? {
        LogsWay::Daemon(mut client) => client
            .rotate_logs(pb::RotateLogsRequest {
                target: Some(target),
            })
            .await
            .map_err(explain)?
            .into_inner(),
        LogsWay::Files(directory) => {
            let document = read_config(&directory.locations.config.path)?;
            let policy = muru_controller::rotation::Policy::from_table(document.logs.as_ref())?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "log rotation is external ([logs] rotate = 'external'): whatever rotates \
                         the files does this, followed by muructl logs reopen"
                    )
                })?;

            muru_controller::log_files::rotate_directly(
                &directory.locations.logs.path,
                directory.existing.as_ref(),
                &target,
                &muru_controller::rotation::Rotator::new(policy),
            )
            .map_err(anyhow::Error::msg)?
        }
    };

    if args.output.json {
        print_answer(&answer)?;
    } else if answer.logs.is_empty() {
        println!("muru: nothing to rotate");
    } else {
        for log in &answer.logs {
            if log.error.is_empty() {
                println!("{} rotated to {}", log.name, log.archive);
            }
        }
    }

    let failures: Vec<String> = answer
        .logs
        .iter()
        .filter(|log| !log.error.is_empty())
        .map(|log| format!("{}: {}", log.name, log.error))
        .collect();

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(failures.join("; "))
    }
}

/// Remove log archives, and the logs of what no longer exists.
pub async fn logs_prune(args: LogsPruneArgs) -> anyhow::Result<()> {
    if args.keep.is_none() && args.older_than.is_none() && !args.orphaned {
        anyhow::bail!("name what to prune: --keep, --older-than or --orphaned");
    }

    let way = logs_way(&args.socket, &args.files).await?;
    let older_than_s = args.older_than.map_or(0, |limit| limit.as_secs());
    let request = pb::PruneLogsRequest {
        keep: args.keep,
        older_than_s,
        orphaned: args.orphaned,
        dry_run: true,
    };

    // Plans and carries out a prune with no daemon running.

    let directly =
        |directory: &LogDirectory, dry_run: bool| -> anyhow::Result<pb::PruneLogsResponse> {
            use muru_controller::log_files;

            if args.orphaned && directory.existing.is_none() {
                anyhow::bail!(
                    "--orphaned needs the state store to tell what no longer exists; name it with \
                 --state-dir"
                );
            }

            let rules = log_files::PruneRules {
                keep: args.keep,
                older_than: (older_than_s > 0)
                    .then(|| std::time::SystemTime::now() - Duration::from_secs(older_than_s)),
                orphaned: args.orphaned,
            };
            let root = &directory.locations.logs.path;
            let logs = log_files::list(root, directory.existing.as_ref())
                .with_context(|| format!("reading {}", root.display()))?;
            let mut removed = log_files::plan_prune(&logs, &rules, None);

            if !dry_run {
                log_files::carry_out_prune(root, &mut removed);
            }

            Ok(log_files::prune_answer(removed, dry_run))
        };

    let plan = match &way {
        LogsWay::Daemon(client) => client
            .clone()
            .prune_logs(request)
            .await
            .map_err(explain)?
            .into_inner(),
        LogsWay::Files(directory) => directly(directory, true)?,
    };

    if args.dry_run || plan.removed.is_empty() {
        if args.output.json {
            return print_answer(&plan);
        }

        if plan.removed.is_empty() {
            println!("muru: nothing to prune");
        }

        for file in &plan.removed {
            println!("{} ({}, {})", file.name, human_size(file.size), file.why);
        }

        if args.dry_run && !plan.removed.is_empty() {
            println!("would free {}", human_size(plan.freed));
        }

        return Ok(());
    }

    for file in &plan.removed {
        eprintln!("  {} ({})", file.name, human_size(file.size));
    }

    let question = format!(
        "Remove {} file{}, freeing {}?",
        plan.removed.len(),
        if plan.removed.len() == 1 { "" } else { "s" },
        human_size(plan.freed)
    );

    if !confirm(&question, args.yes)? {
        eprintln!("muru: nothing was changed");
        return Ok(());
    }

    let answer = match way {
        LogsWay::Daemon(mut client) => client
            .prune_logs(pb::PruneLogsRequest {
                dry_run: false,
                ..request
            })
            .await
            .map_err(explain)?
            .into_inner(),
        LogsWay::Files(directory) => directly(&directory, false)?,
    };

    if args.output.json {
        print_answer(&answer)?;
    } else {
        println!("muru: pruned, freeing {}", human_size(answer.freed));
    }

    let failures: Vec<String> = answer
        .removed
        .iter()
        .filter(|file| !file.error.is_empty())
        .map(|file| format!("{}: {}", file.name, file.error))
        .collect();

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(failures.join("; "))
    }
}

/// The areas `muructl cache` works on.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CacheArea {
    /// The user cache: runtime libraries and certificates.
    User,
    /// The daemon's state directory: its runtime libraries, uv's build
    /// cache and downloaded Pythons.
    Daemon,
}

/// What `muructl cache list` takes.
#[derive(Args, Clone)]
pub struct CacheListArgs {
    /// The area to list; both when none is named.
    #[arg(value_enum)]
    area: Option<CacheArea>,

    #[command(flatten)]
    socket: SocketArgs,

    /// The daemon's and the user cache's locations.
    #[command(flatten)]
    files: DaemonFileArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// The `muructl cache` subcommands.
#[derive(Subcommand)]
pub enum CacheCommand {
    /// Show what the user cache and the daemon's re-creatable data take,
    /// each runtime library by version and interpreter tag, the certificates,
    /// uv's build cache and downloaded Pythons.
    List(Box<CacheListArgs>),

    /// Remove what is stale: runtime libraries of other versions, or of
    /// the version or tag named, and downloaded Pythons no release uses.
    /// Asks first; without a terminal it needs --yes.
    Prune(Box<CachePruneArgs>),

    /// Remove everything in an area that is not in use. It is re-created
    /// on demand. The development certificates are left unless named.
    /// Asks first; without a terminal it needs --yes.
    Clear(Box<CacheClearArgs>),
}

/// What `muructl cache prune` takes.
#[derive(Args, Clone)]
pub struct CachePruneArgs {
    /// The area to prune; both when none is named.
    #[arg(value_enum)]
    area: Option<CacheArea>,

    /// Only runtime libraries of this version, muructl's own included.
    #[arg(long, value_name = "VERSION")]
    version: Option<String>,

    /// Only runtime libraries of this interpreter tag, muructl's own
    /// version included.
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,

    /// Only list what would be removed.
    #[arg(long)]
    dry_run: bool,

    /// Do it without asking.
    #[arg(long)]
    yes: bool,

    #[command(flatten)]
    socket: SocketArgs,

    /// The daemon's and the user cache's locations.
    #[command(flatten)]
    files: DaemonFileArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// What `muructl cache clear` takes.
#[derive(Args, Clone)]
pub struct CacheClearArgs {
    /// The area to clear.
    #[arg(value_enum)]
    area: CacheArea,

    /// Also remove the development certificates, which a browser or the
    /// system may trust.
    #[arg(long)]
    certs: bool,

    /// Only list what would be removed.
    #[arg(long)]
    dry_run: bool,

    /// Do it without asking.
    #[arg(long)]
    yes: bool,

    #[command(flatten)]
    socket: SocketArgs,

    /// The daemon's and the user cache's locations.
    #[command(flatten)]
    files: DaemonFileArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// The user cache directory: --cache-dir, else what `muructl run` uses.
fn user_cache_dir(files: &DaemonFileArgs) -> anyhow::Result<PathBuf> {
    match &files.cache_dir {
        Some(path) => Ok(std::path::absolute(path).unwrap_or_else(|_| path.clone())),
        None => crate::devcert::cache_dir(),
    }
}

/// How a cache command reaches the daemon's area: its daemon when one
/// runs, else its files, with what the state store says the releases use.
enum CacheWay {
    Daemon(Box<muru_proto::admin::v1::admin_client::AdminClient<tonic::transport::Channel>>),
    Files(Box<Locations>, Vec<muru_controller::model::Release>),
}

/// Find how to reach the daemon's area. A --state-dir that is not the
/// running daemon's is refused rather than worked on behind its back.
async fn cache_way(socket: &SocketArgs, files: &DaemonFileArgs) -> anyhow::Result<CacheWay> {
    let locations = daemon_locations(socket, files);
    require_local("cache", locations.instance.as_deref())?;
    let path = socket
        .given_socket()
        .unwrap_or_else(|| locations.admin_socket());

    if no_daemon(socket, &path) {
        let store_path = locations.state_store();

        let releases = if store_path.is_file() {
            muru_controller::store::Store::open_read_only(&store_path)
                .with_context(|| format!("opening {}", store_path.display()))?
                .releases(None)?
        } else {
            Vec::new()
        };

        return Ok(CacheWay::Files(Box::new(locations), releases));
    }

    let mut client = client::connect(&client::Endpoint::unix(&path)).await?;

    if let Some(asked) = &files.state_dir {
        let answer = client
            .list_cache(pb::ListCacheRequest {})
            .await
            .map_err(explain)?
            .into_inner();
        let same = asked.canonicalize().ok() == Path::new(&answer.directory).canonicalize().ok();

        if !same {
            anyhow::bail!(
                "the daemon running at {} keeps its state in {}, not {}",
                path.display(),
                answer.directory,
                asked.display()
            );
        }
    }

    Ok(CacheWay::Daemon(Box::new(client)))
}

/// What the development runs have of the user cache in use.
fn user_cache_in_use() -> muru_controller::cache::InUse {
    let mut in_use = muru_controller::cache::InUse::default();

    // Section 13.8: every live instance of this user says in its registry
    // entry which runtime library directories it uses, whatever run
    // directory it was started with.

    for record in muru_controller::registry::live(&muru_controller::registry::user_registry()) {
        let user = match (&record.kind, &record.app) {
            (muru_controller::registry::Kind::Run, Some(app)) => {
                format!("the development run of {app}")
            }
            (muru_controller::registry::Kind::Serve, Some(app)) => {
                format!("muructl serve of {app}")
            }
            (muru_controller::registry::Kind::Detour, Some(app)) => {
                format!("the detour of {app}")
            }
            _ => format!("instance {}", record.tag()),
        };

        for directory in record.runtime {
            let resolved = std::fs::canonicalize(&directory).unwrap_or(directory);
            in_use.runtime.insert(resolved, user.clone());
        }
    }

    in_use
}

/// The report on the user cache, with what the user's instances use
/// marked.
fn user_cache_report(files: &DaemonFileArgs) -> anyhow::Result<pb::ListCacheResponse> {
    use muru_controller::cache;

    let root = user_cache_dir(files)?;
    let mut entries = cache::user_entries(&root);
    cache::mark_in_use(
        &mut entries,
        &user_cache_in_use(),
        &crate::provision::version(),
    );

    Ok(cache::report(&root, entries))
}

/// The report on the daemon's area, through the daemon or from its files.
async fn daemon_cache_report(way: CacheWay) -> anyhow::Result<pb::ListCacheResponse> {
    match way {
        CacheWay::Daemon(mut client) => Ok(client
            .list_cache(pb::ListCacheRequest {})
            .await
            .map_err(explain)?
            .into_inner()),
        CacheWay::Files(locations, releases) => {
            Ok(muru_controller::cache::state_report(&locations, &releases))
        }
    }
}

/// `muructl cache list`, for scripts: the two areas that were asked for.
#[derive(serde::Serialize)]
struct CacheReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<pb::ListCacheResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon: Option<pb::ListCacheResponse>,
}

/// List the cache areas.
pub async fn cache_list(args: CacheListArgs) -> anyhow::Result<()> {
    let user = if args.area == Some(CacheArea::Daemon) {
        None
    } else {
        Some(user_cache_report(&args.files)?)
    };

    let daemon = if args.area == Some(CacheArea::User) {
        None
    } else {
        Some(daemon_cache_report(cache_way(&args.socket, &args.files).await?).await?)
    };

    if args.output.json {
        return print_answer(&CacheReport { user, daemon });
    }

    for (title, report) in [("USER CACHE", &user), ("DAEMON DATA", &daemon)] {
        let Some(report) = report else {
            continue;
        };

        println!("{title} in {}", report.directory);

        if report.entries.is_empty() {
            println!("  nothing");
        }

        for entry in &report.entries {
            let used = if entry.in_use.is_empty() {
                String::new()
            } else {
                format!("  in use by {}", entry.in_use)
            };

            println!("  {:<40} {:>9}{used}", entry.name, human_size(entry.size));
        }

        println!("  total {}", human_size(report.total));
    }

    Ok(())
}

/// One area's plan or result, and where it goes next.
struct CacheRemoval {
    title: &'static str,
    answer: pb::RemoveCacheResponse,
}

/// A prune's plan or outcome on the user cache.
fn user_cache_prune(
    args: &CachePruneArgs,
    dry_run: bool,
) -> anyhow::Result<pb::RemoveCacheResponse> {
    use muru_controller::cache;

    let rules = cache::PruneRules {
        own_version: crate::provision::version(),
        version: args.version.clone(),
        tag: args.tag.clone(),
    };
    let mut answer = cache::plan_prune(user_cache_report(&args.files)?, &rules, dry_run);

    if !dry_run {
        cache::carry_out(&mut answer);
    }

    Ok(answer)
}

/// A prune's plan or outcome on the daemon's area.
async fn daemon_cache_prune(
    args: &CachePruneArgs,
    way: &CacheWay,
    dry_run: bool,
) -> anyhow::Result<pb::RemoveCacheResponse> {
    match way {
        CacheWay::Daemon(client) => Ok((**client)
            .clone()
            .prune_cache(pb::PruneCacheRequest {
                version: args.version.clone().unwrap_or_default(),
                tag: args.tag.clone().unwrap_or_default(),
                dry_run,
            })
            .await
            .map_err(explain)?
            .into_inner()),
        CacheWay::Files(locations, releases) => Ok(muru_controller::cache::prune_state(
            locations,
            releases,
            args.version.clone(),
            args.tag.clone(),
            dry_run,
        )),
    }
}

/// A clear's plan or outcome on the user cache.
fn user_cache_clear(
    args: &CacheClearArgs,
    dry_run: bool,
) -> anyhow::Result<pb::RemoveCacheResponse> {
    use muru_controller::cache;

    let mut answer = cache::plan_clear(user_cache_report(&args.files)?, args.certs, dry_run);

    if !dry_run {
        cache::carry_out(&mut answer);
    }

    Ok(answer)
}

/// A clear's plan or outcome on the daemon's area.
async fn daemon_cache_clear(
    way: &CacheWay,
    dry_run: bool,
) -> anyhow::Result<pb::RemoveCacheResponse> {
    match way {
        CacheWay::Daemon(client) => Ok((**client)
            .clone()
            .clear_cache(pb::ClearCacheRequest { dry_run })
            .await
            .map_err(explain)?
            .into_inner()),
        CacheWay::Files(locations, releases) => Ok(muru_controller::cache::clear_state(
            locations, releases, dry_run,
        )),
    }
}

/// Print one area's plan and what is left, on standard error unless this
/// is the dry run's answer.
fn show_cache_plan(removal: &CacheRemoval, dry_run: bool) {
    let print = |line: String| {
        if dry_run {
            println!("{line}");
        } else {
            eprintln!("{line}");
        }
    };

    if removal.answer.removed.is_empty() && removal.answer.left.is_empty() {
        return;
    }

    print(format!("{} in {}", removal.title, removal.answer.directory));

    for item in &removal.answer.removed {
        print(format!("  {} ({})", item.name, human_size(item.freed)));
    }

    for item in &removal.answer.left {
        print(format!(
            "  {} is left: in use by {}",
            item.name, item.in_use
        ));
    }
}

/// `muructl cache prune` and `clear`, for scripts: each area asked for.
#[derive(serde::Serialize)]
struct CacheRemovalReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<pb::RemoveCacheResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon: Option<pb::RemoveCacheResponse>,
}

/// Show the plans, ask, and return whether to go on. `notice` is what is
/// said before the question.
fn cache_removal_decided(
    removals: &[CacheRemoval],
    dry_run: bool,
    json: bool,
    yes: bool,
    notice: Option<&str>,
) -> anyhow::Result<bool> {
    let count: usize = removals
        .iter()
        .map(|removal| removal.answer.removed.len())
        .sum();
    let freed: u64 = removals.iter().map(|removal| removal.answer.freed).sum();

    if dry_run || count == 0 {
        if !json {
            for removal in removals {
                show_cache_plan(removal, true);
            }

            if count == 0 {
                println!("muru: nothing to remove");
            } else {
                println!("would free {}", human_size(freed));
            }
        }

        return Ok(false);
    }

    for removal in removals {
        show_cache_plan(removal, false);
    }

    if let Some(notice) = notice {
        eprintln!("{notice}");
    }

    let question = format!(
        "Remove {count} item{}, freeing {}?",
        if count == 1 { "" } else { "s" },
        human_size(freed)
    );

    if confirm(&question, yes)? {
        Ok(true)
    } else {
        eprintln!("muru: nothing was changed");
        Ok(false)
    }
}

/// Report the outcome of a removal, failing if any part of it did.
fn finish_cache_removal(removals: Vec<CacheRemoval>, json: bool) -> anyhow::Result<()> {
    let failures: Vec<String> = removals
        .iter()
        .flat_map(|removal| removal.answer.removed.iter())
        .filter(|item| !item.error.is_empty())
        .map(|item| format!("{}: {}", item.name, item.error))
        .collect();
    let freed: u64 = removals.iter().map(|removal| removal.answer.freed).sum();

    if json {
        let mut report = CacheRemovalReport {
            user: None,
            daemon: None,
        };

        for removal in removals {
            if removal.title == USER_AREA_TITLE {
                report.user = Some(removal.answer);
            } else {
                report.daemon = Some(removal.answer);
            }
        }

        print_answer(&report)?;
    } else {
        println!("muru: removed, freeing {}", human_size(freed));
    }

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(failures.join("; "))
    }
}

const USER_AREA_TITLE: &str = "USER CACHE";
const DAEMON_AREA_TITLE: &str = "DAEMON DATA";

/// Print the plans as `--json` gives them, for a run that stops there.
fn print_cache_plans(removals: Vec<CacheRemoval>) -> anyhow::Result<()> {
    let mut report = CacheRemovalReport {
        user: None,
        daemon: None,
    };

    for removal in removals {
        if removal.title == USER_AREA_TITLE {
            report.user = Some(removal.answer);
        } else {
            report.daemon = Some(removal.answer);
        }
    }

    print_answer(&report)
}

/// Remove what is stale.
pub async fn cache_prune(args: CachePruneArgs) -> anyhow::Result<()> {
    let way = if args.area == Some(CacheArea::User) {
        None
    } else {
        Some(cache_way(&args.socket, &args.files).await?)
    };

    let mut removals = Vec::new();

    if args.area != Some(CacheArea::Daemon) {
        removals.push(CacheRemoval {
            title: USER_AREA_TITLE,
            answer: user_cache_prune(&args, true)?,
        });
    }

    if let Some(way) = &way {
        removals.push(CacheRemoval {
            title: DAEMON_AREA_TITLE,
            answer: daemon_cache_prune(&args, way, true).await?,
        });
    }

    let go_on = cache_removal_decided(&removals, args.dry_run, args.output.json, args.yes, None)?;

    if !go_on {
        return if args.output.json && (args.dry_run || nothing_planned(&removals)) {
            print_cache_plans(removals)
        } else {
            Ok(())
        };
    }

    for removal in &mut removals {
        removal.answer = if removal.title == USER_AREA_TITLE {
            user_cache_prune(&args, false)?
        } else {
            let way = way.as_ref().context("the daemon's area was planned")?;
            daemon_cache_prune(&args, way, false).await?
        };
    }

    finish_cache_removal(removals, args.output.json)
}

/// Whether no area has anything to remove.
fn nothing_planned(removals: &[CacheRemoval]) -> bool {
    removals
        .iter()
        .all(|removal| removal.answer.removed.is_empty())
}

/// Remove everything in an area that is not in use.
pub async fn cache_clear(args: CacheClearArgs) -> anyhow::Result<()> {
    let way = if args.area == CacheArea::User {
        None
    } else {
        Some(cache_way(&args.socket, &args.files).await?)
    };

    let plan = match &way {
        None => CacheRemoval {
            title: USER_AREA_TITLE,
            answer: user_cache_clear(&args, true)?,
        },
        Some(way) => CacheRemoval {
            title: DAEMON_AREA_TITLE,
            answer: daemon_cache_clear(way, true).await?,
        },
    };

    let mut removals = vec![plan];
    let notice = "These are re-created on demand. A runtime library is fetched again when next                   needed, which needs the network or a wheel directory (MURU_WHEEL_DIR).";

    let go_on = cache_removal_decided(
        &removals,
        args.dry_run,
        args.output.json,
        args.yes,
        Some(notice),
    )?;

    if !go_on {
        return if args.output.json && (args.dry_run || nothing_planned(&removals)) {
            print_cache_plans(removals)
        } else {
            Ok(())
        };
    }

    removals[0].answer = match &way {
        None => user_cache_clear(&args, false)?,
        Some(way) => daemon_cache_clear(way, false).await?,
    };

    finish_cache_removal(removals, args.output.json)
}

/// A byte count for a reader, in the units sizes are given in.
pub(crate) fn human_size(bytes: u64) -> String {
    muru_controller::disk::human_size(bytes)
}

/// An access log record as the compact line a development run prints,
/// with the release and host, and the fields the edge's text format
/// writes (group, mount, client class and the rest) when they say something,
/// starting with the record's full time unless `timed` is false. A line
/// that is not a record is printed as it is.
fn format_access_line(line: &str, timed: bool) -> String {
    let Ok(serde_json::Value::Object(record)) = serde_json::from_str::<serde_json::Value>(line)
    else {
        return line.to_owned();
    };

    let text = |key: &str| record.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let number = |key: &str| record.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let millis = |key: &str| {
        muru_edge::access::format_duration(Duration::from_secs_f64(number(key) / 1000.0))
    };
    let bytes =
        muru_edge::access::format_bytes(record.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0));

    let time = if timed {
        format!("{} ", text("time"))
    } else {
        String::new()
    };
    let total = Duration::from_secs_f64(number("total_ms") / 1000.0);

    let total = if record.get("websocket").is_some_and(|v| !v.is_null()) {
        muru_edge::access::format_lifetime(total)
    } else {
        muru_edge::access::format_duration(total)
    };
    let mut out = format!(
        "{time}{} {} {} {bytes} {total}",
        number("status") as u64,
        text("method"),
        text("target"),
    );

    if !text("app").is_empty() {
        out.push_str(&format!(" release={}@{}", text("app"), text("release")));
    }

    if !text("host").is_empty() {
        out.push_str(&format!(" host={}", text("host")));
    }

    // A WebSocket's closing record says who closed, how and what passed;
    // its total is how long the socket was open.

    if let Some(end) = record.get("websocket").and_then(|v| v.as_object()) {
        let field = |key: &str| end.get(key).map(|v| v.to_string()).unwrap_or_default();
        out.push_str(&format!(
            " websocket=closed by={} code={} received={} sent={}",
            end.get("by").and_then(|v| v.as_str()).unwrap_or(""),
            field("code"),
            field("received"),
            field("sent")
        ));
    } else if record.get("worker").is_some_and(|v| !v.is_null()) {
        out.push_str(&format!(
            " queue={} app={}",
            millis("queue_ms"),
            millis("app_ms")
        ));
    } else if number("queue_ms") > 0.0 {
        out.push_str(&format!(" queue={}", millis("queue_ms")));
    }

    if !text("reason").is_empty() {
        out.push_str(&format!(" reason={}", text("reason")));
    }

    let redirects: Vec<&str> = record
        .get("redirects")
        .and_then(|v| v.as_array())
        .map(|targets| targets.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    if !redirects.is_empty() {
        out.push_str(&format!(" redirects={}", redirects.join(",")));
    }

    // The fields after the timings come in the order the edge's own text
    // format writes them, each left out when it says nothing.

    for (key, name) in [
        ("group", "group"),
        ("mount", "mount"),
        ("client_class", "class"),
    ] {
        if !text(key).is_empty() && text(key) != "default" {
            out.push_str(&format!(" {name}={}", text(key)));
        }
    }

    if let Some(worker) = record.get("worker").and_then(|v| v.as_u64()) {
        out.push_str(&format!(" worker={worker}"));
    }

    if record.get("retried").and_then(|v| v.as_bool()) == Some(true) {
        out.push_str(" retried=1");
    }

    if !text("handoff").is_empty() {
        out.push_str(&format!(" handoff={}", text("handoff")));
    }

    out.push_str(&format!(" id={}", text("request_id")));
    out
}

fn format_event(event: &pb::Event) -> String {
    // Section 11.8: a short clock is the reader's local time (issue 625);
    // `--json` keeps the full UTC time.

    let at = std::time::UNIX_EPOCH + Duration::from_millis(event.time_unix_ms);
    let clock = muru_common::log::clock(at);
    let time = clock.split('.').next().unwrap_or_default();
    format!(
        "{:>6} {time} {:<8} {:<20} {:<24} {}",
        event.sequence, event.level, event.r#type, event.subject, event.message
    )
}

fn split_release(text: &str) -> anyhow::Result<(&str, &str)> {
    text.split_once('@')
        .filter(|(app, id)| !app.is_empty() && !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{text:?} is not a release: expected app@release"))
}

fn or_dash(text: &str) -> &str {
    if text.is_empty() { "-" } else { text }
}

/// Print an answer as indented JSON, with the admin API's field names:
/// what `--json` prints.
pub fn print_answer<T: serde::Serialize>(answer: &T) -> anyhow::Result<()> {
    let mut value = serde_json::to_value(answer)?;

    // Section 13.8: an answer from an instance a name chose says which.

    if let (Some(name), Some(object)) = (reached_instance(), value.as_object_mut()) {
        object
            .entry("instance")
            .or_insert_with(|| serde_json::Value::from(name));
    }

    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// A gRPC status as a plain sentence.
pub(crate) fn explain(status: tonic::Status) -> anyhow::Error {
    anyhow::anyhow!("{}", status.message())
}

/// A debug reply past the client's limit says what to narrow; any other
/// error is passed on as `explain` would.
fn explain_debug(op: &str, status: tonic::Status) -> anyhow::Error {
    if status.code() != tonic::Code::OutOfRange {
        return explain(status);
    }

    let hint = match op {
        "profile" => ", or a shorter --for",
        "trace" | "memory" => ", or a smaller --top",
        _ => "",
    };

    anyhow::anyhow!(
        "the reply to debug {op} is larger than the {} MiB muructl accepts; ask one worker with \
         --worker{hint}",
        muru_controller::client::MAX_REPLY_MIB
    )
}

fn process_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 checks for the process and sends nothing.

    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Read the daemon configuration file, which need not exist.
fn read_config(path: &Path) -> anyhow::Result<muru_config::Document> {
    if !path.is_file() {
        return Ok(muru_config::Document::default());
    }

    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_initial_count_is_the_releases_or_one_groups() {
        use super::parse_initial_processes as parse;

        assert_eq!(parse("2"), Ok((None, 2)));
        assert_eq!(parse("jobs=1"), Ok((Some("jobs".to_owned()), 1)));

        for refused in ["0", "jobs=0", "=1", "jobs=", "many"] {
            assert!(parse(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_re_execution_drops_the_flags_of_the_one_before() {
        // Issue 940: a restart after an upgrade, or an upgrade after a
        // restart, says only what it is itself.

        let arguments =
            |text: &str| -> Vec<std::ffi::OsString> { text.split(' ').map(Into::into).collect() };

        assert_eq!(
            super::without_reexec_flags(arguments(
                "daemon run --upgraded-from 1.2.0 --restarted --apply /etc/muru/deploy.d \
                 --held-locks 5,6"
            )),
            arguments("daemon run --apply /etc/muru/deploy.d")
        );
    }

    use super::condition_wait;
    use super::format_access_line;
    use super::limits_lines;
    use super::metric_samples;
    use super::overlay_table;
    use super::pb;

    #[test]
    fn an_access_line_names_what_the_edge_text_format_names() {
        // Issue 760: the client class, like the group and mount, is shown
        // for anything but default, and the other fields follow in the
        // edge's order.

        let record = r#"{"time":"2026-11-17T01:02:03.456Z","request_id":"01ABC","app":"shop","release":"r1","host":"127.0.0.1","group":"api","mount":"default","method":"GET","target":"/","status":200,"reason":"","bytes":58,"worker":0,"retried":true,"queue_ms":0.0,"app_ms":20.0,"total_ms":20.0,"redirects":["/a","/b"],"client_class":"ai-crawler","websocket":null,"handoff":"file"}"#;

        assert_eq!(
            format_access_line(record, false),
            "200 GET / 58B 20ms release=shop@r1 host=127.0.0.1 queue=0.0ms app=20ms \
             redirects=/a,/b group=api class=ai-crawler worker=0 retried=1 handoff=file id=01ABC"
        );

        let plain = record
            .replace(
                r#""client_class":"ai-crawler""#,
                r#""client_class":"default""#,
            )
            .replace(r#""retried":true"#, r#""retried":false"#)
            .replace(r#""handoff":"file""#, r#""handoff":null"#)
            .replace(r#""redirects":["/a","/b"]"#, r#""redirects":[]"#);

        assert_eq!(
            format_access_line(&plain, false),
            "200 GET / 58B 20ms release=shop@r1 host=127.0.0.1 queue=0.0ms app=20ms \
             group=api worker=0 id=01ABC"
        );
    }

    #[test]
    fn a_condition_waiting_out_its_for_says_how_long() {
        let mut condition = pb::ConditionStatus {
            state: "unknown".to_owned(),
            held: "unknown".to_owned(),
            r#for: "10s".to_owned(),
            clear_when: "x < 1".to_owned(),
            clear_for: "1m".to_owned(),
            pending_since_unix_ms: 100_000,
            ..pb::ConditionStatus::default()
        };
        assert_eq!(
            condition_wait(&condition, 104_500).as_deref(),
            Some("when has held for 4s of 10s")
        );

        condition.state = "firing".to_owned();
        assert_eq!(
            condition_wait(&condition, 130_000).as_deref(),
            Some("clear_when has held for 30s of 1m")
        );

        condition.clear_when.clear();
        assert_eq!(
            condition_wait(&condition, 130_000).as_deref(),
            Some("when has been false for 30s of 1m")
        );

        assert_eq!(
            condition_wait(&condition, 100_200).as_deref(),
            Some("when has been false for 0s of 1m")
        );

        condition.pending_since_unix_ms = 0;
        assert_eq!(condition_wait(&condition, 130_000), None);
    }

    #[test]
    fn the_overlay_list_lines_up_however_long_a_state_is() {
        let overlays = [
            pb::OverlayInfo {
                name: "crawler-limits".to_owned(),
                active: true,
                state: "condition".to_owned(),
                trigger: "condition crawler-surge".to_owned(),
                condition: "crawler-surge".to_owned(),
                condition_state: "firing".to_owned(),
                changes: vec!["route shop".to_owned()],
                ..pb::OverlayInfo::default()
            },
            pb::OverlayInfo {
                name: "nightly".to_owned(),
                trigger: "cron '0 2 * * *' for 1h UTC".to_owned(),
                changes: vec!["route shop".to_owned()],
                ..pb::OverlayInfo::default()
            },
        ];
        let lines = overlay_table(&overlays);
        let changes = lines[0].find("CHANGES").unwrap();
        assert!(lines[1].starts_with("crawler-limits           active (condition) condition"));

        for line in &lines[1..] {
            assert_eq!(line.find("route shop"), Some(changes), "{lines:#?}");
        }
    }

    #[test]
    fn app_list_names_the_overlay_that_set_maintenance() {
        let app = |maintenance: bool, overlay: &str| pb::App {
            maintenance,
            maintenance_overlay: overlay.to_owned(),
            ..pb::App::default()
        };

        assert_eq!(super::maintenance_mark(&app(true, "")), " [maintenance]");
        assert_eq!(
            super::maintenance_mark(&app(true, "db-upgrade")),
            " [maintenance by overlay db-upgrade]"
        );
        assert_eq!(super::maintenance_mark(&app(false, "")), "");
        assert_eq!(
            super::maintenance_mark(&app(false, "open")),
            " [maintenance off by overlay open]"
        );
    }

    #[test]
    fn a_skip_with_no_end_says_how_to_try_again() {
        // Issue 755: the next window's start is not when a held overlay
        // is judged again, so it is not shown.

        let skipped = pb::OverlayInfo {
            name: "summer-sale".to_owned(),
            state: "skipped".to_owned(),
            starts_unix_ms: 1_000,
            ..pb::OverlayInfo::default()
        };
        assert_eq!(
            super::overlay_row(&skipped)[1],
            "skipped (activate it by hand, or apply a change, to try again)"
        );

        let idle = pb::OverlayInfo {
            state: String::new(),
            ..skipped
        };
        assert!(super::overlay_row(&idle)[1].starts_with("idle from "));
    }

    #[test]
    fn every_relative_path_in_an_applied_file_is_taken_from_its_directory() {
        let mut document = muru_config::Document::parse(
            r#"
[app.shop.release.r1]
source = "src"
cwd = "run"
python = ".venv/bin/python"
sendfile_roots = ["media"]
mounts.api = { path = "/api", entrypoint = "api:app", sendfile_roots = ["api-media"] }
error_pages = { 503 = { file = "pages/own.html" } }

[host.shop]
names = ["shop.test"]
tls = { certificate = "tls/cert.pem", key = "tls/key.pem" }
error_pages = { 404 = { file = "pages/404.html" } }

[[route]]
name = "root"
host = "shop"
path = "/"
static = "site"
error_pages = { 500 = { file = "/srv/500.html" } }

[overlay.down]
routes.root = { target = { static = "snapshot" }, error_pages = { 503 = { file = "pages/down.html" } } }

[upstream.legacy]
addresses = ["127.0.0.1:9000"]
sendfile_roots = ["legacy"]
tls = { ca = "ca.pem" }
"#,
        )
        .unwrap();
        super::resolve_paths(&mut document, std::path::Path::new("/base"));

        let release = &document.app["shop"].release["r1"];
        assert_eq!(release.source.as_deref(), Some("/base/src"));
        assert_eq!(release.cwd.as_deref(), Some("/base/run"));
        assert_eq!(release.python.as_deref(), Some("/base/.venv/bin/python"));

        // A release's own pages and sendfile_roots are its source
        // directory's.

        assert_eq!(release.sendfile_roots, ["media"]);
        assert_eq!(
            release.mounts["api"].sendfile_roots.as_deref(),
            Some(&["api-media".to_owned()][..])
        );

        assert_eq!(
            release.error_pages["503"].file.as_deref(),
            Some("pages/own.html")
        );

        let host = &document.host["shop"];
        let tls = host.tls.as_ref().unwrap();
        assert_eq!(
            (tls.certificate.as_str(), tls.key.as_str()),
            ("/base/tls/cert.pem", "/base/tls/key.pem")
        );
        assert_eq!(
            host.error_pages["404"].file.as_deref(),
            Some("/base/pages/404.html")
        );

        let route = &document.route[0];
        assert_eq!(route.static_dir.as_deref(), Some("/base/site"));
        assert_eq!(
            route.error_pages["500"].file.as_deref(),
            Some("/srv/500.html")
        );

        let overlay = &document.overlay["down"].routes["root"];
        assert_eq!(
            overlay.target.as_ref().unwrap().static_dir.as_deref(),
            Some("/base/snapshot")
        );
        assert_eq!(
            overlay.error_pages.as_ref().unwrap()["503"].file.as_deref(),
            Some("/base/pages/down.html")
        );

        let upstream = &document.upstream["legacy"];
        assert_eq!(upstream.sendfile_roots, ["/base/legacy"]);
        assert_eq!(
            upstream.tls.as_ref().unwrap().ca.as_deref(),
            Some("/base/ca.pem")
        );
    }

    #[test]
    fn the_limits_are_printed_as_the_guide_shows_them() {
        let limits = pb::CgroupLimits {
            cgroup: "/sys/fs/cgroup".to_owned(),
            cpu_quota: 2.0,
            cpus: 64,
            memory_max: 1 << 30,
            memory_used: 586 << 20,
            memory_left: 438 << 20,
            headroom: 128 << 20,
            workers_memory: 296 << 20,
            host_memory: 250 << 20,
            guard: "watching".to_owned(),
            guard_note: String::new(),
            last_replaced: "shop@r1 worker 0 (pid 52)".to_owned(),
            last_replaced_unix_ms: (7 * 3600 + 40 * 60 + 12) * 1000,
        };

        assert_eq!(
            limits_lines(&limits),
            "cpu      2 CPUs from cpu.max\n\
             memory   memory.max 1.0GB, 586MB used, 438MB left, headroom 128MB\n\
             \x20        workers 296MB, host 250MB\n\
             guard    watching; it last replaced shop@r1 worker 0 (pid 52) at 07:40:12 UTC\n"
        );
    }

    #[test]
    fn discovery_reports_a_denied_run_directory_only_with_nothing_else_to_offer() {
        use std::path::{Path, PathBuf};

        use crate::run_admin::RunInfo;

        let run = |socket: &str, project: &str| RunInfo {
            pid: 1,
            app: "app".to_owned(),
            project: PathBuf::from(project),
            started_from: None,
            listen: "127.0.0.1:8080".to_owned(),
            detour: false,
            socket: PathBuf::from(socket),
            runtime: Vec::new(),
            started_unix_ms: 0,
        };
        let reach = |run: &RunInfo| format!("--socket {}", run.socket.display());
        let daemon = PathBuf::from("/user/admin.sock");
        let denied = PathBuf::from("/run/muru/admin.sock");
        let found = |runs: &[RunInfo], here, listening, denied: Option<&Path>| {
            super::discovered(
                runs,
                here,
                daemon.clone(),
                listening,
                denied.map(Path::to_path_buf),
                &reach,
            )
            .expect("found")
            .socket
        };
        let mine = [run("/user/runs/8080/admin.sock", "/mine")];

        // A non-member's own run or daemon wins, nothing said about the
        // directory.

        let here = Some(Path::new("/mine"));
        assert_eq!(found(&mine, here, true, Some(&denied)), mine[0].socket);
        assert_eq!(found(&mine, None, false, Some(&denied)), mine[0].socket);
        assert_eq!(found(&[], None, true, Some(&denied)), daemon);

        // With nothing else the denied socket is the answer, and only then.

        assert_eq!(found(&[], None, false, Some(&denied)), denied);
        assert_eq!(found(&[], None, false, None), daemon);
    }

    #[test]
    fn discovery_reaches_the_newest_of_several_runs_of_a_project_and_names_the_others() {
        use std::path::{Path, PathBuf};

        use crate::run_admin::RunInfo;

        let run = |port: u16, started_unix_ms: u64| RunInfo {
            pid: 1,
            app: "shop".to_owned(),
            project: PathBuf::from("/work/shop"),
            started_from: None,
            listen: format!("127.0.0.1:{port}"),
            detour: false,
            socket: PathBuf::from(format!("/user/runs/{port}/admin.sock")),
            runtime: Vec::new(),
            started_unix_ms,
        };

        // Listed by port, the older run on the lower one.

        let reach = |run: &RunInfo| format!("--instance run:{}", &run.listen[10..]);
        let runs = [run(8080, 1_000), run(8090, 2_000)];
        let found = super::discovered(
            &runs,
            Some(Path::new("/work/shop")),
            PathBuf::from("/user/admin.sock"),
            true,
            None,
            &reach,
        )
        .expect("found");
        assert_eq!(found.socket, runs[1].socket);
        let note = found.note.expect("a note");
        assert!(
            note.starts_with(
                "2 development runs of this project are listening; reaching the newest, shop on http://127.0.0.1:8090;"
            ),
            "{note}"
        );
        assert!(
            note.ends_with("; --instance run:8080 reaches the other"),
            "{note}"
        );

        // One run is reached quietly.

        let one = super::discovered(
            &runs[..1],
            Some(Path::new("/work/shop")),
            PathBuf::from("/user/admin.sock"),
            true,
            None,
            &reach,
        )
        .expect("found");
        assert_eq!((one.socket, one.note), (runs[0].socket.clone(), None));
    }

    #[test]
    fn discovery_finds_a_run_from_where_it_was_started_and_says_when_the_daemon_wins() {
        use std::path::{Path, PathBuf};

        use crate::run_admin::RunInfo;

        let shop = RunInfo {
            pid: 1,
            app: "shop".to_owned(),
            project: PathBuf::from("/work/shop"),
            started_from: Some(PathBuf::from("/work")),
            listen: "127.0.0.1:18380".to_owned(),
            detour: false,
            socket: PathBuf::from("/user/runs/18380/admin.sock"),
            runtime: Vec::new(),
            started_unix_ms: 0,
        };
        let other = RunInfo {
            app: "api".to_owned(),
            project: PathBuf::from("/work"),
            started_from: Some(PathBuf::from("/elsewhere")),
            listen: "127.0.0.1:18381".to_owned(),
            detour: false,
            socket: PathBuf::from("/user/runs/18381/admin.sock"),
            ..shop.clone()
        };
        let daemon = PathBuf::from("/user/admin.sock");
        let reach = |run: &RunInfo| format!("--instance run:{}", &run.listen[10..]);
        let found = |runs: &[RunInfo], here: &str| {
            super::discovered(
                runs,
                Some(Path::new(here)),
                daemon.clone(),
                true,
                None,
                &reach,
            )
            .expect("found")
        };

        // The directory a run was started from reaches it ahead of the
        // daemon, quietly; a run whose project is that directory wins over it.

        let started = found(std::slice::from_ref(&shop), "/work");
        assert_eq!(started.socket, shop.socket);
        assert!(started.note.is_none());

        let both = [shop.clone(), other.clone()];
        assert_eq!(found(&both, "/work").socket, other.socket);

        // Elsewhere the daemon answers, and the note names the run passed
        // over and how to reach it.

        let elsewhere = found(std::slice::from_ref(&shop), "/tmp");
        assert_eq!(elsewhere.socket, daemon);
        let note = elsewhere.note.expect("a note");
        assert!(note.starts_with("using the daemon;"), "{note}");
        assert!(note.contains("shop on http://127.0.0.1:18380"), "{note}");
        assert!(
            note.contains("--instance run:18380, or the command run from /work/shop"),
            "{note}"
        );

        // Of several, only how many, and where they are all named.

        let several = found(&both, "/tmp").note.expect("a note");
        assert_eq!(
            several,
            "using the daemon; 2 development runs are also listening: pass --instance to \
             reach one, as muructl instance list names them"
        );

        // With no daemon, the runs to choose from, one to a line.

        let chosen = super::discovered(
            &both,
            Some(Path::new("/tmp")),
            daemon.clone(),
            false,
            None,
            &reach,
        )
        .expect_err("two runs and no daemon")
        .to_string();
        assert_eq!(
            chosen,
            "no daemon is running and 2 development runs are; choose one, or run the command \
             from its project directory:\n  --instance run:18380  shop on http://127.0.0.1:18380 \
             from /work/shop\n  --instance run:18381  api on http://127.0.0.1:18381 from /work"
        );
    }

    #[test]
    fn a_directory_that_may_not_be_entered_is_told_from_an_absent_one() {
        use std::os::unix::fs::PermissionsExt;

        // SAFETY: geteuid has no preconditions.

        if unsafe { libc::geteuid() } == 0 {
            return; // root enters anything, so there is nothing to refuse
        }

        let directory = tempfile::tempdir().expect("temporary directory");
        let locked = directory.path().join("locked");
        std::fs::create_dir(&locked).expect("mkdir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        assert!(super::may_not_enter(&locked.join("serve.json")));
        assert!(!super::may_not_enter(&directory.path().join("absent/x")));
        assert!(!super::may_not_enter(&directory.path().join("x")));

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    }

    #[test]
    fn a_debug_reply_past_the_limit_says_what_to_narrow() {
        let status = tonic::Status::out_of_range("Error, decoded message length too large");
        let message = super::explain_debug("profile", status).to_string();
        assert!(
            message.starts_with("the reply to debug profile is larger than"),
            "{message}"
        );
        assert!(
            message.ends_with("--worker, or a shorter --for"),
            "{message}"
        );

        let other = tonic::Status::not_found("no such release");
        assert_eq!(
            super::explain_debug("stacks", other).to_string(),
            "no such release"
        );
    }

    #[test]
    fn the_workers_column_says_what_runs_beside_the_shape() {
        let release = pb::Release {
            processes: 1,
            threads: 4,
            kind: "workers".to_owned(),
            ..Default::default()
        };
        assert_eq!(super::workers_column(&release), "- (1x4)");

        let at_zero = pb::Release {
            host_running: true,
            ..release.clone()
        };
        assert_eq!(super::workers_column(&at_zero), "0 (1x4)");

        let serving = pb::Release {
            workers: vec![pb::Worker::default(), pb::Worker::default()],
            ..at_zero
        };
        assert_eq!(super::workers_column(&serving), "2 (1x4)");

        let server = pb::Release {
            kind: "http".to_owned(),
            instances: 2,
            profile: "gunicorn".to_owned(),
            ..Default::default()
        };
        assert_eq!(super::workers_column(&server), "- (2 gunicorn)");

        let running = pb::Release {
            server_instances: vec![
                pb::ServerInstance {
                    pid: 7,
                    ..Default::default()
                },
                pb::ServerInstance::default(),
            ],
            ..server
        };
        assert_eq!(super::workers_column(&running), "1 (2 gunicorn)");
    }

    #[tokio::test]
    async fn output_that_cannot_be_written_stops_the_command_and_waits_for_it() {
        let dir = tempfile::tempdir().expect("a directory");
        let cleaned = dir.path().join("cleaned");
        let script = format!(
            "trap 'sleep 0.5; echo done > {}; exit 0' TERM; while :; do echo tick; sleep 0.1; done",
            cleaned.display()
        );
        let running = muru_controller::commands::run(
            muru_controller::commands::Prepared {
                app: "shop".to_owned(),
                release: String::new(),
                label: "exec".to_owned(),
                argv: vec!["sh".to_owned(), "-c".to_owned(), script],
                env: Vec::new(),
                unset: Vec::new(),
                clean: false,
                cwd: dir.path().to_owned(),
                identity: None,
                log: None,
                timeout: None,
                declared: None,
                exclusive: false,
                inputs: String::new(),
                secrets: None,
            },
            std::sync::Arc::new(muru_controller::commands::Unrecorded),
        )
        .expect("started");
        let (_signalled, signals) = tokio::sync::mpsc::unbounded_channel();

        // The first line cannot be written, as to a pipe nothing reads.

        let mut tried = 0;
        let relayed = super::relay(running, signals, |_| {
            tried += 1;
            Err(anyhow::anyhow!("Broken pipe (os error 32)"))
        })
        .await;

        let error = relayed.expect_err("the output could not be written");
        assert!(format!("{error:#}").contains("Broken pipe"), "{error:#}");
        assert_eq!(
            tried, 1,
            "output that could not be written is not tried again"
        );
        assert!(
            cleaned.exists(),
            "the command was stopped with SIGTERM, and waited for"
        );
    }

    #[test]
    fn muructl_exits_with_the_status_or_1_for_a_stopped_command_that_exited_with_0() {
        let ending = |exit_code, ok| super::pb::CommandOutput {
            finished: true,
            exit_code,
            ok,
            ..Default::default()
        };

        assert_eq!(super::exit_status(&ending(0, true)), 0);
        assert_eq!(super::exit_status(&ending(3, false)), 3);
        assert_eq!(super::exit_status(&ending(143, false)), 143);
        assert_eq!(super::exit_status(&ending(0, false)), 1);
    }

    #[test]
    fn metric_samples_carry_names_types_labels_and_values() {
        let text = "\
# HELP muru_requests_total Requests answered.
# TYPE muru_requests_total counter
muru_requests_total{app=\"shop\",status=\"200\"} 42
# TYPE muru_request_seconds histogram
muru_request_seconds_bucket{le=\"+Inf\"} 7
muru_request_seconds_count 7
up 1 1700000000000
";
        let samples = metric_samples(text);

        assert_eq!(samples.len(), 4);
        assert_eq!(samples[0]["name"], "muru_requests_total");
        assert_eq!(samples[0]["type"], "counter");
        assert_eq!(samples[0]["labels"]["status"], "200");
        assert_eq!(samples[0]["value"], 42.0);
        assert_eq!(samples[1]["type"], "histogram");
        assert_eq!(samples[1]["labels"]["le"], "+Inf");
        assert_eq!(samples[2]["type"], "histogram");
        assert_eq!(samples[3]["type"], "untyped");
        assert_eq!(samples[3]["value"], 1.0);
    }

    #[test]
    fn label_values_unescape_and_values_json_cannot_hold_stay_text() {
        let samples = metric_samples("odd{path=\"a\\\"b\\\\c\\nd\",x=\"1\"} NaN\n");

        assert_eq!(samples[0]["labels"]["path"], "a\"b\\c\nd");
        assert_eq!(samples[0]["labels"]["x"], "1");
        assert_eq!(samples[0]["value"], "NaN");
    }

    #[test]
    fn an_apply_the_edge_did_not_take_exits_1() {
        let applied = super::pb::ConfigReport {
            applied: true,
            changes: vec![super::pb::ConfigChange {
                action: "update-route".to_owned(),
                subject: "route shop /".to_owned(),
                detail: String::new(),
                outcome: "updated".to_owned(),
            }],
            ..Default::default()
        };
        assert_eq!(super::exit_code(&applied, false), 0);

        // Issue #441: every change was made, and the edge refused the
        // snapshot that carries them.

        let refused = super::pb::ConfigReport {
            snapshot_error: "the edge refused the snapshot: no".to_owned(),
            ..applied
        };
        assert_eq!(super::exit_code(&refused, false), 1);
    }
}

#[cfg(test)]
mod logs_list_tests {
    use clap::Parser;

    use super::{LogsListArgs, pb};

    /// The arguments as `muructl logs list` parses them.
    #[derive(Parser)]
    struct Command {
        #[command(flatten)]
        args: LogsListArgs,
    }

    fn report() -> pb::ListLogsResponse {
        let log = |name: &str, kind: &str, app: &str, release: &str, hosts: &[&str]| pb::LogInfo {
            name: name.to_owned(),
            kind: kind.to_owned(),
            app: app.to_owned(),
            release: release.to_owned(),
            hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
            total: 10,
            ..pb::LogInfo::default()
        };

        pb::ListLogsResponse {
            directory: "/logs".to_owned(),
            logs: vec![
                log("access.log", "access", "", "", &[]),
                log("apps/shop/r1.log", "release", "shop", "r1", &[]),
                log("apps/shop/r2.log", "release", "shop", "r2", &[]),
                log("apps/shopfront/r1.log", "release", "shopfront", "r1", &[]),
                log("intranet.log", "host", "", "", &["intranet"]),
            ],
            total: 50,
            orphans_known: true,
        }
    }

    fn narrowed(arguments: &[&str]) -> anyhow::Result<(Vec<String>, u64)> {
        let command =
            Command::try_parse_from(std::iter::once("list").chain(arguments.iter().copied()))?;
        let mut report = report();
        command.args.narrow(&mut report)?;

        Ok((
            report.logs.into_iter().map(|log| log.name).collect(),
            report.total,
        ))
    }

    #[test]
    fn a_target_narrows_the_list_and_its_total() {
        assert_eq!(narrowed(&[]).unwrap().0.len(), 5);
        assert_eq!(
            narrowed(&["shop"]).unwrap(),
            (
                vec!["apps/shop/r1.log".to_owned(), "apps/shop/r2.log".to_owned()],
                20
            )
        );
        assert_eq!(narrowed(&["shop@r2"]).unwrap().0, ["apps/shop/r2.log"]);
        assert_eq!(narrowed(&["--access"]).unwrap().0, ["access.log"]);
        assert_eq!(
            narrowed(&["--host", "intranet"]).unwrap().0,
            ["intranet.log"]
        );

        let refused = narrowed(&["shop@r9"]).unwrap_err().to_string();
        assert_eq!(refused, "no log for shop@r9, in /logs");
        assert!(narrowed(&["--host", "shop"]).is_err());
        assert!(narrowed(&["shop@"]).is_err());
        assert!(
            narrowed(&["shop", "--access"]).is_err(),
            "one target at a time"
        );
    }

    #[test]
    fn a_records_text_marks_a_failed_span_an_event_and_segments() {
        let span = |name: &str, failed: bool| pb::PanelSpan {
            name: name.to_owned(),
            start_ms: 1.0,
            duration_ms: 2.0,
            failed,
            ..Default::default()
        };
        let segment = |start_ms: f64| pb::PanelSegment {
            start_ms,
            duration_ms: 0.5,
            ..Default::default()
        };
        let record = pb::RequestRecord {
            panels: vec![pb::Panel {
                title: "Trace".to_owned(),
                spans: vec![pb::PanelSpan {
                    children: vec![
                        span("SELECT 1", false),
                        span("lookup", true),
                        pb::PanelSpan {
                            at_ms: Some(2.5),
                            ..span("response started", false)
                        },
                        pb::PanelSpan {
                            segments: vec![segment(1.0), segment(2.0)],
                            ..span("waiting", false)
                        },
                    ],
                    ..span("GET /books", true)
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let text = super::request_text(&record);
        let lines: Vec<&str> = text.lines().skip_while(|line| *line != "Trace").collect();

        assert!(
            lines[1].starts_with("  GET /books") && lines[1].ends_with("  failed"),
            "{text}"
        );
        assert!(
            lines[2].starts_with("    SELECT 1") && !lines[2].ends_with("failed"),
            "{text}"
        );
        assert!(lines[3].ends_with("from 1.0ms  failed"), "{text}");
        assert!(lines[4].ends_with("  at 2.5ms"), "{text}");
        assert!(
            lines[5].ends_with("1.0ms in 2 segments  from 1.0ms"),
            "{text}"
        );
    }

    #[test]
    fn a_capture_s_stack_is_indented_below_its_name() {
        let row = |name: &str, value: &str| pb::PanelRow {
            cells: vec![name.to_owned(), value.to_owned()],
        };
        let record = pb::RequestRecord {
            panels: vec![pb::Panel {
                title: "Errors".to_owned(),
                tables: vec![pb::PanelTable {
                    caption: "request.soft_timeout at 14:26:36.016".to_owned(),
                    columns: vec!["name".to_owned(), "value".to_owned()],
                    rows: vec![
                        row("worker", "0"),
                        row(
                            "stack",
                            "  File \"shop.py\", line 180, in report\n    total += 1\n",
                        ),
                        row(
                            "thread pool-1",
                            "  File \"a.py\", line 1, in run\n    wait()",
                        ),
                    ],
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        // Issue 780: every line of a value of several lines sits under
        // its name, as the panel's own values do.

        let text = super::request_text(&record);
        let lines: Vec<&str> = text.lines().skip_while(|line| *line != "Errors").collect();
        assert_eq!(
            lines[1..8],
            [
                "  request.soft_timeout at 14:26:36.016",
                "    worker: 0",
                "    stack",
                "        File \"shop.py\", line 180, in report",
                "          total += 1",
                "    thread pool-1",
                "        File \"a.py\", line 1, in run",
            ],
            "{text}"
        );
    }
}
