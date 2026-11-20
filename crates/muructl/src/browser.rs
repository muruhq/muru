//! Opening a URL in the browser on this machine (design specification
//! section 13.1, issue 815), for `muructl open` and `muructl run --open`.
//!
//! The launcher is `$BROWSER` when set, on every platform; otherwise
//! `open` on macOS, and on Linux `wslview` under WSL, then `xdg-open`, then
//! `gio open`. The URL is passed as an argument, never through a shell.
//! Some `xdg-open` setups wait until the browser exits, so the launcher is
//! given a second to fail and is otherwise left running, its output
//! thrown away. Where there is nowhere to open it, on Linux with no
//! display, under SSH with no `$BROWSER`, or with no launcher found, the
//! URL is printed with a line saying so.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// What the environment says about where a browser could be opened.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    /// `$BROWSER`.
    pub browser: Option<String>,
    /// `DISPLAY` or `WAYLAND_DISPLAY` is set.
    pub display: bool,
    /// `SSH_CONNECTION` is set.
    pub ssh: bool,
    /// Running under the Windows Subsystem for Linux.
    pub wsl: bool,
    /// macOS rather than Linux.
    pub macos: bool,
}

impl Environment {
    /// The environment of this process.
    pub fn current() -> Self {
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());

        Self {
            browser: std::env::var("BROWSER")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            display: set("DISPLAY") || set("WAYLAND_DISPLAY"),
            ssh: set("SSH_CONNECTION"),
            wsl: set("WSL_DISTRO_NAME")
                || std::fs::read_to_string("/proc/version")
                    .is_ok_and(|version| version.to_ascii_lowercase().contains("microsoft")),
            macos: cfg!(target_os = "macos"),
        }
    }
}

/// How the URL is to be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launcher {
    /// Run this program with these arguments and the URL after them.
    Run(PathBuf, Vec<String>),
    /// Nowhere: print the URL, with why.
    Nowhere(String),
}

/// The launcher for `environment`, with `find` looking a program up on
/// the PATH.
pub fn choose(environment: &Environment, find: impl Fn(&str) -> Option<PathBuf>) -> Launcher {
    // $BROWSER is the user's choice, wherever it runs; it may carry
    // arguments of its own, split on whitespace.

    if let Some(browser) = &environment.browser {
        let mut words = browser.split_whitespace();

        if let Some(program) = words.next() {
            let program = if program.contains('/') {
                Some(PathBuf::from(program))
            } else {
                find(program)
            };

            return match program {
                Some(program) => Launcher::Run(program, words.map(str::to_owned).collect()),
                None => {
                    Launcher::Nowhere(format!("$BROWSER names {browser:?}, which was not found"))
                }
            };
        }
    }

    if environment.ssh {
        return Launcher::Nowhere(
            "this is an SSH session with no $BROWSER; the address is this machine's, so reaching \
             it from another machine needs port forwarding"
                .to_owned(),
        );
    }

    if environment.macos {
        return match find("open") {
            Some(open) => Launcher::Run(open, Vec::new()),
            None => Launcher::Nowhere("open was not found".to_owned()),
        };
    }

    if !environment.display && !environment.wsl {
        return Launcher::Nowhere(
            "there is no display (DISPLAY and WAYLAND_DISPLAY are not set)".to_owned(),
        );
    }

    let candidates: &[(&str, &[&str])] = if environment.wsl {
        &[("wslview", &[]), ("xdg-open", &[]), ("gio", &["open"])]
    } else {
        &[("xdg-open", &[]), ("gio", &["open"])]
    };

    for (program, arguments) in candidates {
        if let Some(found) = find(program) {
            return Launcher::Run(
                found,
                arguments.iter().map(|word| (*word).to_owned()).collect(),
            );
        }
    }

    Launcher::Nowhere("no launcher was found (xdg-open or gio)".to_owned())
}

/// A program on the PATH.
pub fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path)
        .is_ok_and(|found| found.is_file() && found.permissions().mode() & 0o111 != 0)
}

/// How long a launcher has to fail before it is left running.
const FAIL_WITHIN: Duration = Duration::from_secs(1);

/// Open `url` in the browser on this machine, or say why it cannot be,
/// printing the URL either way. Never fails the command: with nowhere to
/// open it, the URL is what the user needs.
pub fn open(url: &str) {
    match choose(&Environment::current(), on_path) {
        Launcher::Run(program, arguments) => match launch(&program, &arguments, url) {
            Ok(()) => println!("muru: opened {url}"),
            Err(why) => nowhere(url, &why),
        },
        Launcher::Nowhere(why) => nowhere(url, &why),
    }
}

/// Run the launcher with the URL as its last argument, its output thrown
/// away, giving it a moment to fail.
fn launch(program: &Path, arguments: &[String], url: &str) -> Result<(), String> {
    let mut child = std::process::Command::new(program)
        .args(arguments)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("{} could not be run: {error}", program.display()))?;

    let deadline = std::time::Instant::now() + FAIL_WITHIN;

    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{} failed ({status})", program.display())),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error.to_string()),
        }
    }

    // Still running, as some launchers do until the browser exits: it is
    // left to it.

    Ok(())
}

/// Say the URL could not be opened, and why.
fn nowhere(url: &str, why: &str) {
    println!("{url}");
    muru_common::muru_eprintln!("no browser could be opened: {why}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(names: &'static [&'static str]) -> impl Fn(&str) -> Option<PathBuf> {
        move |program| {
            names
                .contains(&program)
                .then(|| PathBuf::from(format!("/usr/bin/{program}")))
        }
    }

    #[test]
    fn browser_is_taken_first_with_its_arguments() {
        let environment = Environment {
            browser: Some("firefox --new-tab".to_owned()),
            ssh: true,
            ..Environment::default()
        };

        assert_eq!(
            choose(&environment, found(&["firefox"])),
            Launcher::Run(
                PathBuf::from("/usr/bin/firefox"),
                vec!["--new-tab".to_owned()]
            )
        );
    }

    #[test]
    fn each_platform_has_its_launcher_and_nowhere_says_why() {
        let macos = Environment {
            macos: true,
            ..Environment::default()
        };
        assert_eq!(
            choose(&macos, found(&["open"])),
            Launcher::Run(PathBuf::from("/usr/bin/open"), Vec::new())
        );

        let desktop = Environment {
            display: true,
            ..Environment::default()
        };
        assert_eq!(
            choose(&desktop, found(&["gio"])),
            Launcher::Run(PathBuf::from("/usr/bin/gio"), vec!["open".to_owned()])
        );

        let wsl = Environment {
            wsl: true,
            ..Environment::default()
        };
        assert_eq!(
            choose(&wsl, found(&["wslview", "xdg-open"])),
            Launcher::Run(PathBuf::from("/usr/bin/wslview"), Vec::new())
        );

        assert!(matches!(
            choose(&Environment::default(), found(&["xdg-open"])),
            Launcher::Nowhere(why) if why.contains("no display")
        ));
        let ssh = Environment {
            ssh: true,
            display: true,
            ..Environment::default()
        };
        assert!(matches!(
            choose(&ssh, found(&["xdg-open"])),
            Launcher::Nowhere(why) if why.contains("port forwarding")
        ));
    }

    #[test]
    fn a_launcher_is_given_the_url_as_an_argument() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let script = directory.path().join("browser");
        let written = directory.path().join("opened");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", written.display()),
        )
        .expect("written");

        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        let url = "http://localhost:8000/a?b=c&d='e'";
        launch(&script, &[], url).expect("launched");
        assert_eq!(std::fs::read_to_string(&written).expect("read"), url);

        let failing = directory.path().join("failing");
        std::fs::write(&failing, "#!/bin/sh\nexit 3\n").expect("written");

        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        assert!(launch(&failing, &[], url).is_err());
    }
}
