//! Where the daemon keeps things (design specification section 13.8).
//!
//! Muru has no fixed paths. Each location is resolved from the first of:
//! an explicit option, an environment variable, a directory systemd
//! provides, or the default for the mode the daemon runs in.
//!
//! The runtime location holds sockets and nothing else (issue 948, ADR
//! 0083). A socket's path is limited to about a hundred bytes, so the
//! location is short, which on macOS means `/tmp`, where a nightly job
//! deletes regular files nobody has touched for three days and leaves
//! sockets alone. Every regular file a running instance keeps (its lock,
//! its pid file, the records a restarted daemon re-adopts from) is in
//! `run` under the state directory instead, in the same shape: a host's
//! sockets are in `RUN/h/N` and its files in `STATE/run/h/N`, where N is
//! the release's number.

use std::env;
use std::path::PathBuf;

/// The mode the daemon runs in, which decides the default locations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Root, or `--system`: the system locations.
    System,
    /// A user's own daemon: the user locations.
    User,
}

/// The explicit options a command line may give.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The daemon configuration file.
    pub config: Option<PathBuf>,
    /// The state directory.
    pub state_dir: Option<PathBuf>,
    /// The runtime directory.
    pub run_dir: Option<PathBuf>,
    /// The log directory.
    pub log_dir: Option<PathBuf>,
    /// The cache directory.
    pub cache_dir: Option<PathBuf>,
    /// The system locations, whoever runs the daemon.
    pub system: bool,
    /// A named instance, whose locations all follow from its name
    /// (section 13.8, issue 810); `user` and `system` name the default
    /// daemons.
    pub instance: Option<String>,
}

/// The longest an instance's name may be, so it fits in titles and keeps
/// socket paths within the platform's limit.
pub const NAME_LIMIT: usize = 24;

/// The names of the default daemons, which no named instance may have.
pub const RESERVED_NAMES: [&str; 2] = ["user", "system"];

/// Check an instance's name: 1 to 24 characters of `[a-z0-9-]`, starting
/// with a letter.
pub fn check_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= NAME_LIMIT
        && name.starts_with(|first: char| first.is_ascii_lowercase())
        && name
            .chars()
            .all(|char| char.is_ascii_lowercase() || char.is_ascii_digit() || char == '-');

    if valid {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not an instance name: 1 to {NAME_LIMIT} lowercase letters, digits and \
             hyphens, starting with a letter"
        ))
    }
}

/// The directory, under a location's area, that a named instance's
/// locations go in.
pub const INSTANCES_DIR: &str = "instances";

/// The variable that names the daemon's configuration file.
pub const CONFIG_VARIABLE: &str = "MURU_DAEMON_CONFIG";

/// The name that variable had, which is refused so a daemon never passes
/// its configuration over without a word.
const RENAMED_CONFIG_VARIABLE: &str = "MURU_CONFIG";

/// Refuse the daemon's configuration variable under its earlier name,
/// unless the new one is set too.
pub fn check_renamed() -> Result<(), String> {
    let set = |name: &str| env::var_os(name).is_some_and(|value| !value.is_empty());

    if set(RENAMED_CONFIG_VARIABLE) && !set(CONFIG_VARIABLE) {
        return Err(format!(
            "{RENAMED_CONFIG_VARIABLE} is set, and the daemon's configuration file is now named \
             by {CONFIG_VARIABLE}; set that instead"
        ));
    }

    Ok(())
}

/// One resolved location and the rule that chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The path.
    pub path: PathBuf,
    /// Which rule of section 13.8 chose it: option, environment, systemd or default.
    pub rule: &'static str,
}

/// Every location the daemon uses.
#[derive(Debug, Clone)]
pub struct Locations {
    /// The mode the defaults came from.
    pub mode: Mode,
    /// The named instance the defaults are for, when there is one.
    pub instance: Option<String>,
    /// The daemon configuration file, which need not exist.
    pub config: Location,
    /// The state store, built environments and other durable files.
    pub state: Location,
    /// The sockets: the admin socket, the edge's and each host's. Nothing
    /// else is kept here (see [`Locations::files`]).
    pub run: Location,
    /// Log files, when file sinks are configured.
    pub logs: Location,
    /// Re-creatable files.
    pub cache: Location,
}

/// The longest a Unix socket path may be on this platform, its NUL
/// included: sun_path is 104 bytes on macOS and 108 on Linux.
pub const SOCKET_PATH_LIMIT: usize = if cfg!(target_os = "macos") { 104 } else { 108 };

/// The directory, in the runtime location and in the runtime files, that
/// holds one directory per host, named by a number.
pub const HOSTS_DIR: &str = "h";

/// The directory in the runtime location that holds the edge's sockets:
/// the edge's own, so that it can bind them as the service user.
pub const EDGE_DIR: &str = "e";

/// The directory in the state directory that holds the runtime files.
pub const FILES_DIR: &str = "run";

/// The longest a socket's path is below the run directory, whatever the
/// application, the release and its groups are called: a host's
/// directory with a number of five digits, and in it the socket of a
/// named group's worker, with two digits each for the group, the worker
/// and its generation.
pub const SOCKET_PATH_OVERHEAD: usize = "/h/99999".len() + "/worker-99-99-g99.sock".len();

/// What a development run's own directory adds to that, between the run
/// directory and its hosts: `/run-` and the eight digits of its short ID.
pub const RUN_PATH_OVERHEAD: usize = "/run-01234567".len();

/// The directory of host `number` under `base`, which is a runtime
/// location or its runtime files.
pub fn host_dir(base: &std::path::Path, number: u64) -> PathBuf {
    base.join(HOSTS_DIR).join(number.to_string())
}

/// The two directories of one host, or of the instances of one managed
/// server release: its sockets in the runtime location, and its files
/// among the runtime files (section 13.8, issue 948).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDirs {
    /// Where its sockets are bound: the workers', its control socket and
    /// its relay's.
    pub sockets: PathBuf,
    /// Where its regular files are: its relay's record, and what the
    /// host and its starter leave for each other.
    pub files: PathBuf,
}

impl HostDirs {
    /// The directories of host `number` under a runtime location and its
    /// runtime files.
    pub fn numbered(sockets: &std::path::Path, files: &std::path::Path, number: u64) -> Self {
        Self {
            sockets: host_dir(sockets, number),
            files: host_dir(files, number),
        }
    }

    /// One directory as both, where nothing deletes files by age: the
    /// run directory of `muructl serve`, which is the container's.
    pub fn together(directory: &std::path::Path) -> Self {
        Self {
            sockets: directory.to_path_buf(),
            files: directory.to_path_buf(),
        }
    }

    /// Make both directories. They are made whenever they are needed and
    /// never assumed, since an empty directory under `/tmp` is removed by
    /// age as a file is.
    pub fn create(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.sockets)?;
        std::fs::create_dir_all(&self.files)
    }

    /// Remove both directories and what is in them.
    pub fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.sockets);
        let _ = std::fs::remove_dir_all(&self.files);
    }
}

/// Whether the daemon that owns the run directory `run` is one from
/// before the runtime files moved to the state directory (issue 948): it
/// keeps its lock in a file there, which this version never makes.
pub fn earlier_daemon_owns(run: &std::path::Path) -> bool {
    run.join("daemon.lock").is_file()
}

/// Whether the run directory `run` holds the records of an edge or of
/// hosts that such an earlier daemon started and did not stop: this
/// version reads its records from the state directory, so it could
/// neither take them over nor know they are there.
pub fn earlier_daemon_left(run: &std::path::Path) -> bool {
    run.join("edge.toml").is_file() || run.join("apps").is_dir()
}

/// Check that the longest socket path under `sockets`, the directory of a
/// daemon or of a development run, fits the platform's limit.
pub fn check_socket_room(sockets: &std::path::Path) -> Result<(), String> {
    let length = sockets.as_os_str().len();
    let longest = length + SOCKET_PATH_OVERHEAD;

    // The limit counts the NUL that ends the path.

    if longest >= SOCKET_PATH_LIMIT {
        return Err(format!(
            "the run directory {} is {length} bytes long, and a worker's socket path under it \
             can be {longest} bytes, past the {} this platform allows; give --run-dir or \
             MURU_RUN_DIR a shorter path",
            sockets.display(),
            SOCKET_PATH_LIMIT - 1
        ));
    }

    Ok(())
}

impl Locations {
    /// Check the run directory before anything binds a socket under it.
    /// It must be absolute, since the workers and the edge bind their
    /// sockets by the paths they are given wherever they run, and short
    /// enough that the longest socket path under it is within the
    /// platform's limit. No path under it carries a name, so a directory
    /// that passes here never fails later for what a release is called.
    pub fn check_run_dir(&self) -> Result<(), String> {
        let run = &self.run.path;

        if run.is_relative() {
            return Err(format!(
                "the run directory {} is a relative path; --run-dir or MURU_RUN_DIR must be \
                 absolute",
                run.display()
            ));
        }

        check_socket_room(run)
    }

    /// Resolve every location for the given options and environment.
    pub fn resolve(options: &Options) -> Self {
        // `system` as an instance's name is the system daemon, and `user`
        // the default one of whoever runs.

        let named = options
            .instance
            .clone()
            .filter(|name| !RESERVED_NAMES.contains(&name.as_str()));

        let mode = if options.system || is_root() || options.instance.as_deref() == Some("system") {
            Mode::System
        } else {
            Mode::User
        };
        let home = env::var_os("HOME").map(PathBuf::from);
        let name = named.as_deref();

        let state = pick(
            options.state_dir.as_ref(),
            "MURU_STATE_DIR",
            "STATE_DIRECTORY",
            || named_state(mode, home.as_deref(), name),
        );
        let config = pick(
            options.config.as_ref(),
            CONFIG_VARIABLE,
            "CONFIGURATION_DIRECTORY",
            || named_config(mode, home.as_deref(), name),
        );
        let run = pick(
            options.run_dir.as_ref(),
            "MURU_RUN_DIR",
            "RUNTIME_DIRECTORY",
            || named_under(default_run(mode), name),
        );
        let logs = pick(
            options.log_dir.as_ref(),
            "MURU_LOG_DIR",
            "LOGS_DIRECTORY",
            || named_under(default_logs(mode, home.as_deref()), name),
        );
        let cache = pick(
            options.cache_dir.as_ref(),
            "MURU_CACHE_DIR",
            "CACHE_DIRECTORY",
            || default_cache(mode, home.as_deref()),
        );

        // A relative state, config, log or cache directory is taken from
        // where the command runs, and made absolute here: a release's host
        // starts in the release's source, where the relative path to the
        // runtime library under the state directory would not resolve. The
        // run directory is refused instead (check_run_dir).

        let [state, config, logs, cache] = [state, config, logs, cache].map(absolute);

        // A configuration directory holds the file; a file is used as is.

        let config = if config.path.is_dir() || config.rule == "systemd" {
            Location {
                path: config.path.join("muru.toml"),
                rule: config.rule,
            }
        } else {
            config
        };

        Self {
            mode,
            instance: named,
            config,
            state,
            run,
            logs,
            cache,
        }
    }

    /// Whose daemon this is, as its process title, its edge's and its row
    /// in `muructl status --processes` name it (section 2.1): `user` or
    /// `system`, or the state directory when it is not the default for
    /// the mode, so that daemons kept apart by `--state-dir` are told
    /// apart.
    pub fn scope(&self) -> String {
        let home = env::var_os("HOME").map(PathBuf::from);
        let default = std::path::absolute(named_state(
            self.mode,
            home.as_deref(),
            self.instance.as_deref(),
        ));

        if default.is_ok_and(|default| default == self.state.path) {
            self.instance
                .clone()
                .unwrap_or_else(|| self.mode_word().to_owned())
        } else {
            self.state.path.display().to_string()
        }
    }

    /// The instance's name in the registry (section 13.8): `user` or
    /// `system` for the default daemon of the mode, none for a daemon
    /// whose locations were given by flags.
    pub fn instance_name(&self) -> Option<String> {
        let scope = self.scope();
        let named = self.instance.as_deref().unwrap_or(self.mode_word());
        (scope == named).then_some(scope)
    }

    /// The directory holding a named instance's definition, its
    /// `instance.toml` and `muru.toml`; none for a default daemon.
    pub fn definition_dir(&self) -> Option<PathBuf> {
        let home = env::var_os("HOME").map(PathBuf::from);
        let name = self.instance.as_deref()?;
        Some(
            config_area(self.mode, home.as_deref())
                .join(INSTANCES_DIR)
                .join(name),
        )
    }

    /// `user` or `system`, for the mode.
    pub fn mode_word(&self) -> &'static str {
        match self.mode {
            Mode::System => "system",
            Mode::User => "user",
        }
    }

    /// The admin socket inside the runtime directory.
    pub fn admin_socket(&self) -> PathBuf {
        self.run.path.join("admin.sock")
    }

    /// The state store inside the state directory.
    pub fn state_store(&self) -> PathBuf {
        self.state.path.join("muru.sqlite")
    }

    /// The daemon's own log file, for a background daemon.
    pub fn daemon_log(&self) -> PathBuf {
        self.logs.path.join("daemon.log")
    }

    /// The edge process's own output.
    pub fn edge_log(&self) -> PathBuf {
        self.logs.path.join("edge.log")
    }

    /// The runtime files: every regular file the running daemon keeps,
    /// in `run` under the state directory, never in the runtime location
    /// (issue 948).
    pub fn files(&self) -> PathBuf {
        self.state.path.join(FILES_DIR)
    }

    /// The daemon's pid file, for a background daemon.
    pub fn pid_file(&self) -> PathBuf {
        self.files().join("daemon.pid")
    }

    /// The record of the running edge, which a restarted daemon re-adopts
    /// it from.
    pub fn edge_record(&self) -> PathBuf {
        self.files().join("edge.toml")
    }

    /// The directory of the edge's sockets.
    pub fn edge_sockets(&self) -> PathBuf {
        self.run.path.join(EDGE_DIR)
    }

    /// The directory of the edge's own files: the rate limits' buckets.
    pub fn edge_files(&self) -> PathBuf {
        self.files().join("edge")
    }

    /// The sockets of the host of release number `number`.
    pub fn host_sockets(&self, number: u64) -> PathBuf {
        host_dir(&self.run.path, number)
    }

    /// The files of the host of release number `number`: its record, its
    /// relay's, and the secrets delivered as files.
    pub fn host_files(&self, number: u64) -> PathBuf {
        host_dir(&self.files(), number)
    }

    /// Both directories of the host of release number `number`.
    pub fn host_dirs(&self, number: u64) -> HostDirs {
        HostDirs::numbered(&self.run.path, &self.files(), number)
    }

    /// The access log the edge writes, as JSON lines.
    pub fn access_log(&self) -> PathBuf {
        self.logs.path.join("access.log")
    }

    /// Where a release's host writes its own and its workers' output.
    pub fn release_log(&self, app: &str, id: &str) -> PathBuf {
        self.logs
            .path
            .join("apps")
            .join(app)
            .join(format!("{id}.log"))
    }
}

/// The first rule that applies: option, environment, systemd, default.
fn pick(
    option: Option<&PathBuf>,
    variable: &str,
    systemd: &str,
    default: impl FnOnce() -> PathBuf,
) -> Location {
    if let Some(path) = option {
        return Location {
            path: path.clone(),
            rule: "option",
        };
    }

    if let Some(path) = env::var_os(variable).filter(|value| !value.is_empty()) {
        return Location {
            path: PathBuf::from(path),
            rule: "environment",
        };
    }

    // systemd lists several directories separated by colons; the first
    // is the unit's own.

    if let Some(value) = env::var_os(systemd).filter(|value| !value.is_empty()) {
        let first = value
            .to_string_lossy()
            .split(':')
            .next()
            .unwrap_or_default()
            .to_owned();
        return Location {
            path: PathBuf::from(first),
            rule: "systemd",
        };
    }

    Location {
        path: default(),
        rule: "default",
    }
}

/// The location with its path made absolute against the working directory.
fn absolute(location: Location) -> Location {
    Location {
        path: std::path::absolute(&location.path).unwrap_or(location.path),
        rule: location.rule,
    }
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.

    unsafe { libc::geteuid() == 0 }
}

fn home_or_cwd(home: Option<&std::path::Path>) -> PathBuf {
    home.map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn xdg(variable: &str, home: Option<&std::path::Path>, fallback: &str) -> PathBuf {
    env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_or_cwd(home).join(fallback))
}

/// The configuration area of the mode: where the default daemon's file
/// is, and the catalogue of named instances under `instances`.
pub fn config_area(mode: Mode, home: Option<&std::path::Path>) -> PathBuf {
    default_config(mode, home)
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// The configuration area of whoever runs, or the system's.
pub fn caller_config_area(system: bool) -> PathBuf {
    let mode = if system || is_root() {
        Mode::System
    } else {
        Mode::User
    };
    config_area(mode, env::var_os("HOME").map(PathBuf::from).as_deref())
}

/// A location's default, under `instances/NAME` for a named instance.
fn named_under(path: PathBuf, name: Option<&str>) -> PathBuf {
    match name {
        Some(name) => path.join(INSTANCES_DIR).join(name),
        None => path,
    }
}

/// The configuration file's default: `CONFIG/instances/NAME/muru.toml`
/// for a named instance.
fn named_config(mode: Mode, home: Option<&std::path::Path>, name: Option<&str>) -> PathBuf {
    match name {
        Some(name) => config_area(mode, home)
            .join(INSTANCES_DIR)
            .join(name)
            .join("muru.toml"),
        None => default_config(mode, home),
    }
}

/// The state directory's default: on macOS a named instance's needs a
/// `state` directory of its own, since the default configuration and
/// state locations are one directory there.
fn named_state(mode: Mode, home: Option<&std::path::Path>, name: Option<&str>) -> PathBuf {
    match name {
        Some(name) if mode == Mode::User && cfg!(target_os = "macos") => {
            named_under(default_state(mode, home), Some(name)).join("state")
        }
        name => named_under(default_state(mode, home), name),
    }
}

fn default_config(mode: Mode, home: Option<&std::path::Path>) -> PathBuf {
    match mode {
        Mode::System => PathBuf::from("/etc/muru/muru.toml"),
        Mode::User if cfg!(target_os = "macos") => {
            home_or_cwd(home).join("Library/Application Support/muru/muru.toml")
        }
        Mode::User => xdg("XDG_CONFIG_HOME", home, ".config").join("muru/muru.toml"),
    }
}

fn default_state(mode: Mode, home: Option<&std::path::Path>) -> PathBuf {
    match mode {
        Mode::System => PathBuf::from("/var/lib/muru"),
        Mode::User if cfg!(target_os = "macos") => {
            home_or_cwd(home).join("Library/Application Support/muru")
        }
        Mode::User => xdg("XDG_STATE_HOME", home, ".local/state").join("muru"),
    }
}

/// The user state location (section 13.8, issue 849): where a user's own
/// daemon keeps its state, `MURU_STATE_DIR` when it is set, whoever runs,
/// root included. `muructl run` keeps each project's data and static
/// directories under it, never in the project.
pub fn user_state() -> PathBuf {
    env::var_os("MURU_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            default_state(
                Mode::User,
                env::var_os("HOME").map(PathBuf::from).as_deref(),
            )
        })
}

/// Whether the caller is root, whose instances are the system's.
pub fn caller_is_root() -> bool {
    is_root()
}

/// The state location of the account that runs, which depends on
/// nothing but who the user is: the home directory is the password
/// database's and not `HOME`, and no option, `MURU_STATE_DIR` or
/// `XDG_STATE_HOME` moves it. The user's instance registry is there
/// (section 13.8, issue 948), so that every process of the user finds
/// the same one, whatever environment it was started with. None for a
/// user with no account or no home directory, as a container given an
/// arbitrary user ID has.
pub fn account_state() -> Option<PathBuf> {
    // SAFETY: getuid has no preconditions and cannot fail.

    let uid = unsafe { libc::getuid() };
    let home = crate::host::Identity::account_home(uid).filter(|home| home.is_dir())?;
    Some(account_state_in(&home))
}

/// Where the account with the home directory `home` keeps its state, as
/// [`account_state`] gives the caller's.
pub fn account_state_in(home: &std::path::Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/muru")
    } else {
        home.join(".local/state/muru")
    }
}

/// The user's default runtime location, whatever an option or variable
/// says: where the registry of a user with no home directory is.
pub fn default_user_runtime() -> PathBuf {
    default_run(Mode::User)
}

/// Where each development run keeps its runtime files, one directory a
/// run: `run` under the user state location.
pub fn user_run_files() -> PathBuf {
    user_state().join(FILES_DIR)
}

fn default_run(mode: Mode) -> PathBuf {
    match mode {
        Mode::System => PathBuf::from("/run/muru"),
        // Short, for the 104-byte limit on socket paths. Only sockets go
        // here: macOS deletes regular files in /tmp by age.
        Mode::User if cfg!(target_os = "macos") => {
            // SAFETY: getuid has no preconditions and cannot fail.

            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/muru-{uid}"))
        }
        Mode::User => match env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
            Some(dir) => PathBuf::from(dir).join("muru"),
            None => {
                // SAFETY: getuid has no preconditions and cannot fail.

                let uid = unsafe { libc::getuid() };
                PathBuf::from(format!("/tmp/muru-{uid}"))
            }
        },
    }
}

fn default_logs(mode: Mode, home: Option<&std::path::Path>) -> PathBuf {
    match mode {
        Mode::System => PathBuf::from("/var/log/muru"),
        Mode::User if cfg!(target_os = "macos") => home_or_cwd(home).join("Library/Logs/muru"),
        Mode::User => xdg("XDG_STATE_HOME", home, ".local/state").join("muru/logs"),
    }
}

fn default_cache(mode: Mode, home: Option<&std::path::Path>) -> PathBuf {
    match mode {
        Mode::System => PathBuf::from("/var/cache/muru"),
        Mode::User if cfg!(target_os = "macos") => home_or_cwd(home).join("Library/Caches/muru"),
        Mode::User => xdg("XDG_CACHE_HOME", home, ".cache").join("muru"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_win_over_defaults() {
        let options = Options {
            state_dir: Some(PathBuf::from("/tmp/state")),
            run_dir: Some(PathBuf::from("/tmp/run")),
            ..Options::default()
        };
        let locations = Locations::resolve(&options);

        assert_eq!(locations.state.path, PathBuf::from("/tmp/state"));
        assert_eq!(locations.state.rule, "option");
        assert_eq!(
            locations.admin_socket(),
            PathBuf::from("/tmp/run/admin.sock")
        );
        assert_eq!(locations.cache.rule, "default");
    }

    #[test]
    fn sockets_are_in_the_run_directory_and_every_file_in_the_state_directory() {
        let locations = Locations::resolve(&Options {
            state_dir: Some(PathBuf::from("/srv/state")),
            run_dir: Some(PathBuf::from("/tmp/run")),
            ..Options::default()
        });

        assert_eq!(locations.host_sockets(142), PathBuf::from("/tmp/run/h/142"));
        assert_eq!(locations.edge_sockets(), PathBuf::from("/tmp/run/e"));

        for file in [
            locations.pid_file(),
            locations.edge_record(),
            locations.edge_files(),
            locations.host_files(142),
        ] {
            assert!(file.starts_with("/srv/state/run"), "{}", file.display());
        }

        assert_eq!(
            locations.host_files(142),
            PathBuf::from("/srv/state/run/h/142")
        );
    }

    #[test]
    fn the_longest_socket_path_does_not_depend_on_any_name() {
        // The longest socket of a host, and of an edge and a managed
        // server, against the overhead the check allows for.

        let host = host_dir(std::path::Path::new(""), 99_999);

        for socket in [
            "worker-99-99-g99.sock",
            "worker-99-g99.sock",
            "control.sock",
            "relay.sock",
            "output.sock",
            "server-99.sock",
            "relay-99.sock",
        ] {
            let length = host.join(socket).as_os_str().len();
            assert!(length <= SOCKET_PATH_OVERHEAD, "{socket}: {length}");
        }

        assert_eq!(SOCKET_PATH_OVERHEAD, 30);
        assert!("/e/edge-999-listener.sock".len() <= SOCKET_PATH_OVERHEAD);

        // A directory is accepted exactly while the longest path fits.

        let limit = SOCKET_PATH_LIMIT - 1;
        let fits = format!("/{}", "d".repeat(limit - SOCKET_PATH_OVERHEAD - 1));
        let over = format!("{fits}d");
        assert!(check_socket_room(std::path::Path::new(&fits)).is_ok());
        let refused = check_socket_room(std::path::Path::new(&over)).unwrap_err();
        assert!(refused.contains("shorter path"), "{refused}");
    }

    #[test]
    fn relative_directories_are_made_absolute_but_the_run_directory_is_not() {
        let options = Options {
            state_dir: Some(PathBuf::from("muru/state")),
            cache_dir: Some(PathBuf::from("muru/cache")),
            run_dir: Some(PathBuf::from("muru/run")),
            ..Options::default()
        };
        let locations = Locations::resolve(&options);
        let here = env::current_dir().unwrap();

        assert_eq!(locations.state.path, here.join("muru/state"));
        assert_eq!(locations.cache.path, here.join("muru/cache"));
        assert_eq!(locations.state.rule, "option");
        assert!(locations.check_run_dir().is_err());
    }

    #[test]
    fn a_named_instance_puts_every_location_under_its_name() {
        let named = Locations::resolve(&Options {
            instance: Some("dev".to_owned()),
            ..Options::default()
        });
        let default = Locations::resolve(&Options::default());

        // Unless a variable or systemd already decides the location.

        for (location, base) in [(&named.run, &default.run), (&named.logs, &default.logs)] {
            if location.rule == "default" {
                assert_eq!(location.path, base.path.join("instances/dev"));
            }
        }

        if named.state.rule == "default" {
            let expected = if cfg!(target_os = "macos") {
                default.state.path.join("instances/dev/state")
            } else {
                default.state.path.join("instances/dev")
            };
            assert_eq!(named.state.path, expected);
            assert_eq!(named.scope(), "dev");
            assert_eq!(named.instance_name().as_deref(), Some("dev"));
        }

        assert_eq!(named.cache.path, default.cache.path);
        assert!(
            named
                .definition_dir()
                .expect("a definition")
                .ends_with("instances/dev")
        );

        // `user` is the default daemon.

        let user = Locations::resolve(&Options {
            instance: Some("user".to_owned()),
            ..Options::default()
        });
        assert_eq!(user.state.path, default.state.path);
        assert!(user.definition_dir().is_none());
    }

    #[test]
    fn names_are_short_lowercase_and_start_with_a_letter() {
        assert!(check_name("dev").is_ok());
        assert!(check_name("staging-2").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("2dev").is_err());
        assert!(check_name("Dev").is_err());
        assert!(check_name("dev_1").is_err());
        assert!(check_name(&"a".repeat(25)).is_err());
    }

    #[test]
    fn system_mode_uses_the_system_paths() {
        let options = Options {
            system: true,
            ..Options::default()
        };
        let locations = Locations::resolve(&options);

        assert_eq!(locations.mode, Mode::System);
        assert_eq!(locations.state.path, PathBuf::from("/var/lib/muru"));
        assert_eq!(locations.run.path, PathBuf::from("/run/muru"));
        assert!(locations.config.path.ends_with("muru.toml"));
    }
}
