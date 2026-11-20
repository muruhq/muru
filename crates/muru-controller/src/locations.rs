//! Where the daemon keeps things (design specification section 13.8).
//!
//! Muru has no fixed paths. Each location is resolved from the first of:
//! an explicit option, an environment variable, a directory systemd
//! provides, or the default for the mode the daemon runs in.

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
    /// The admin socket and per-release run directories.
    pub run: Location,
    /// Log files, when file sinks are configured.
    pub logs: Location,
    /// Re-creatable files.
    pub cache: Location,
}

/// The longest a Unix socket path may be on this platform, its NUL
/// included: sun_path is 104 bytes on macOS and 108 on Linux.
pub const SOCKET_PATH_LIMIT: usize = if cfg!(target_os = "macos") { 104 } else { 108 };

/// What a worker's socket path takes below the run directory besides the
/// application's name: `/apps/APP/RID/RID/worker-N-gG.sock` with a release
/// id of four characters and two digits each for the worker and its
/// generation.
const SOCKET_PATH_OVERHEAD: usize =
    "/apps/".len() + "/r999".len() * 2 + "/worker-99-g99.sock".len();

/// The room an application's name must have in the socket path for the
/// run directory to be accepted at all.
const SHORTEST_NAME_ROOM: usize = 8;

impl Locations {
    /// Check the run directory before anything binds a socket under it.
    /// It must be absolute, since the workers and the edge bind their
    /// sockets by the paths they are given wherever they run, and short
    /// enough to leave a worker's socket path within the platform's
    /// limit, which it otherwise exceeds only once a release starts.
    pub fn check_run_dir(&self) -> Result<(), String> {
        let run = &self.run.path;

        if run.is_relative() {
            return Err(format!(
                "the run directory {} is a relative path; --run-dir or MURU_RUN_DIR must be \
                 absolute",
                run.display()
            ));
        }

        let length = run.as_os_str().len();
        let room = SOCKET_PATH_LIMIT.saturating_sub(length + SOCKET_PATH_OVERHEAD + 1);

        if room < SHORTEST_NAME_ROOM {
            return Err(format!(
                "the run directory {} is {length} bytes long, which leaves {room} bytes for an \
                 application's name in its workers' socket paths, limited to \
                 {SOCKET_PATH_LIMIT} bytes on this platform; give --run-dir or MURU_RUN_DIR a \
                 shorter path",
                run.display()
            ));
        }

        Ok(())
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

    /// The daemon's pid file, for a background daemon.
    pub fn pid_file(&self) -> PathBuf {
        self.run.path.join("daemon.pid")
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

/// The default runtime location for whoever runs: the system one for
/// root, the user's otherwise, whatever an option or variable says. The
/// instance registry lives there (section 13.8).
pub fn default_runtime() -> PathBuf {
    default_run(if is_root() { Mode::System } else { Mode::User })
}

fn default_run(mode: Mode) -> PathBuf {
    match mode {
        Mode::System => PathBuf::from("/run/muru"),
        // Short, for the 104-byte limit on socket paths.
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
