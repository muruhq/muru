//! Detours (design specification section 9.13, ADR 0075): a development
//! run that a daemon routes to as a release of kind `detour` of an
//! application, for as long as the run holds its admin call open. The
//! daemon keeps them in memory alone; this module has what it keeps and
//! the choices that need no daemon.

use std::path::PathBuf;

use muru_proto::admin::v1 as pb;
use tokio::sync::mpsc;

/// What a detour's release ID starts with.
pub const RELEASE_PREFIX: &str = "dev-";

/// The release name that stands for an application's one detour, as in
/// `shop@detour`.
pub const ALIAS: &str = "detour";

/// The source an overlay a detour owns is listed with (section 9.10).
pub const OVERLAY_SOURCE: &str = "detour";

/// How many hexadecimal digits of the run's instance ID a release ID
/// takes at first.
const SHORT_DIGITS: usize = 4;

/// Why a daemon without the setting refuses a detour.
pub fn refusal_off(config: &str) -> String {
    format!(
        "detours are off on this daemon: set detours = true in the [daemon] table of {config} \
         and restart the daemon"
    )
}

/// Why a system daemon refuses detours, as a setting and as a request.
pub const SYSTEM_REFUSAL: &str = "a system daemon takes no detours: its edge serves real \
                                  traffic, which a detour would send to a process owned by \
                                  whoever ran muructl detour run";

/// Why a daemon refuses detours as a setting, for a system daemon whose
/// file turns them on.
pub fn setting_refusal(enabled: bool, mode: crate::locations::Mode) -> Option<String> {
    (enabled && mode == crate::locations::Mode::System)
        .then(|| format!("detours = true in the [daemon] table is refused: {SYSTEM_REFUSAL}"))
}

/// Why a daemon refuses to open a detour: a system daemon never takes
/// one, and a user daemon only with the setting.
pub fn opening_refusal(
    enabled: bool,
    mode: crate::locations::Mode,
    config: &std::path::Path,
) -> Option<String> {
    if mode == crate::locations::Mode::System {
        return Some(SYSTEM_REFUSAL.to_owned());
    }

    (!enabled).then(|| refusal_off(&config.display().to_string()))
}

/// What a detour takes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Traffic {
    /// Previews alone, by its cookie or header.
    #[default]
    Preview,
    /// Every request for the application.
    All,
    /// A percentage of the application's clients.
    Share(u32),
    /// The requests of a client class the daemon defines.
    Class(String),
}

impl Traffic {
    /// Read a choice as the command line and the admin call write it:
    /// `preview` (or nothing), `all`, `share=N` or `class=NAME`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();

        match text {
            "" | "preview" => return Ok(Self::Preview),
            "all" => return Ok(Self::All),
            _ => {}
        }

        match text.split_once('=') {
            Some(("share", value)) => match value.trim_end_matches('%').parse::<u32>() {
                Ok(share) if (1..=100).contains(&share) => Ok(Self::Share(share)),
                _ => Err(format!(
                    "share={value} is not a percentage from 1 to 100, as share=10"
                )),
            },
            Some(("class", name)) if !name.is_empty() && !name.contains(char::is_whitespace) => {
                Ok(Self::Class(name.to_owned()))
            }
            Some(("class", name)) => Err(format!("class={name} does not name a client class")),
            _ => Err(format!(
                "traffic {text:?} is not preview, all, share=N or class=NAME"
            )),
        }
    }

    /// The choice as it was written.
    pub fn word(&self) -> String {
        match self {
            Self::Preview => "preview".to_owned(),
            Self::All => "all".to_owned(),
            Self::Share(share) => format!("share={share}"),
            Self::Class(class) => format!("class={class}"),
        }
    }
}

/// What a run asks for when it opens a detour.
#[derive(Debug, Clone)]
pub struct Opening {
    /// The application.
    pub app: String,
    /// The run's socket.
    pub socket: PathBuf,
    /// The run's label, or empty.
    pub label: String,
    /// The run's instance ID, or empty.
    pub instance_id: String,
    /// The run's pid.
    pub pid: u32,
    /// The run's checkout.
    pub directory: String,
    /// What it takes.
    pub traffic: Traffic,
}

impl Opening {
    /// The request as the run sent it, checked.
    pub fn from_message(request: pb::OpenDetourRequest) -> Result<Self, String> {
        if !muru_config::valid_name(&request.app) {
            return Err(format!("{:?} is not an application name", request.app));
        }

        let socket = PathBuf::from(&request.socket);

        if !socket.is_absolute() {
            return Err(format!(
                "the detour's socket {:?} is not an absolute path",
                request.socket
            ));
        }

        Ok(Self {
            app: request.app,
            socket,
            label: request.label,
            instance_id: request.instance_id,
            pid: request.pid,
            directory: request.directory,
            traffic: Traffic::parse(&request.traffic)?,
        })
    }
}

/// The updates a detour's stream carries to its run.
pub type Updates = mpsc::Sender<Result<pb::DetourMessage, tonic::Status>>;

/// An open detour, as the daemon holds it.
#[derive(Debug, Clone)]
pub struct Detour {
    /// What the run asked for.
    pub opening: Opening,
    /// Its release ID, `dev-7f3a`.
    pub release: String,
    /// The preview secret, 128 random bits in hexadecimal.
    pub secret: String,
    /// When it opened, Unix milliseconds.
    pub opened_unix_ms: i64,
    /// Which opening it is, so the end of one stream never closes a
    /// detour another stream opened since.
    pub serial: u64,
    /// The stream to its run.
    pub updates: Updates,
}

impl Detour {
    /// The application.
    pub fn app(&self) -> &str {
        &self.opening.app
    }

    /// The detour as listings show it, without its secret.
    pub fn message(&self) -> pb::Detour {
        pb::Detour {
            app: self.opening.app.clone(),
            release: self.release.clone(),
            label: self.opening.label.clone(),
            instance_id: self.opening.instance_id.clone(),
            pid: self.opening.pid,
            directory: self.opening.directory.clone(),
            traffic: self.opening.traffic.word(),
            opened_unix_ms: self.opened_unix_ms,
            socket: self.opening.socket.display().to_string(),
        }
    }

    /// The upstream address the edge reaches it at.
    pub fn address(&self) -> String {
        format!("unix:{}", self.opening.socket.display())
    }

    /// Who opened it, for events: `pid 4123 in ~/src/shop, label wip`.
    pub fn describe(&self) -> String {
        let mut text = format!(
            "pid {} in {}",
            self.opening.pid,
            crate::registry::home_shortened(std::path::Path::new(&self.opening.directory))
        );

        if !self.opening.label.is_empty() {
            text.push_str(&format!(", label {}", self.opening.label));
        }

        text
    }
}

/// The release ID for a run's detour: `dev-` and the first four
/// hexadecimal digits of its instance ID, so a run that opens its detour
/// again keeps its name, longer while `taken` says another detour of the
/// application has it. A run with no ID is given random digits.
pub fn release_name(instance_id: &str, taken: impl Fn(&str) -> bool) -> String {
    let digits: String = instance_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|digit| digit.to_ascii_lowercase())
        .collect();

    let digits = if digits.len() >= SHORT_DIGITS {
        digits
    } else {
        crate::registry::InstanceId::generate().to_string()
    };

    for length in SHORT_DIGITS..=digits.len() {
        let name = format!("{RELEASE_PREFIX}{}", &digits[..length]);

        if !taken(&name) {
            return name;
        }
    }

    // Every prefix of the ID is taken, which only a second opening by
    // the same run could do; a random suffix keeps them apart.

    let fresh = crate::registry::InstanceId::generate().to_string();
    format!("{RELEASE_PREFIX}{digits}-{}", &fresh[..SHORT_DIGITS])
}

/// Whether a release ID is a detour's.
pub fn is_detour_release(release: &str) -> bool {
    release.starts_with(RELEASE_PREFIX)
}

/// Why a release ID a deploy or an apply gives is refused, when it is
/// one kept for detours: `detour`, or one starting with `dev-`.
pub fn reserved_release(id: &str) -> Option<String> {
    (id == ALIAS || is_detour_release(id)).then(|| {
        format!(
            "{id:?} is kept for detours: release IDs starting with {RELEASE_PREFIX} and the ID \
             {ALIAS} name a development run's detour"
        )
    })
}

/// A new preview secret: 128 random bits in hexadecimal. Unlike an
/// instance ID it has no fallback, since a guessable secret would let
/// anyone reach the detour.
pub fn secret() -> Result<String, String> {
    use ring::rand::SecureRandom as _;

    let mut bytes = [0u8; 16];

    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "the system's random source failed".to_owned())?;

    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traffic_reads_each_choice_and_refuses_the_rest() {
        assert_eq!(Traffic::parse("").unwrap(), Traffic::Preview);
        assert_eq!(Traffic::parse("preview").unwrap(), Traffic::Preview);
        assert_eq!(Traffic::parse("all").unwrap(), Traffic::All);
        assert_eq!(Traffic::parse("share=10").unwrap(), Traffic::Share(10));
        assert_eq!(Traffic::parse("share=25%").unwrap(), Traffic::Share(25));
        assert_eq!(
            Traffic::parse("class=crawlers").unwrap(),
            Traffic::Class("crawlers".to_owned())
        );

        for wrong in [
            "share=0",
            "share=101",
            "share=x",
            "class=",
            "half",
            "pin=r1",
        ] {
            assert!(Traffic::parse(wrong).is_err(), "{wrong} was taken");
        }

        for choice in ["preview", "all", "share=10", "class=crawlers"] {
            assert_eq!(Traffic::parse(choice).unwrap().word(), choice);
        }
    }

    #[test]
    fn a_release_takes_four_digits_of_the_id_and_more_when_they_are_taken() {
        let id = "7f3a91c2d4e5f60718293a4b5c6d7e8f";

        assert_eq!(release_name(id, |_| false), "dev-7f3a");
        assert_eq!(release_name(id, |name| name == "dev-7f3a"), "dev-7f3a9");
        assert!(release_name("", |_| false).starts_with("dev-"));
        assert!(is_detour_release("dev-7f3a"));
        assert!(!is_detour_release("r42"));
        assert!(reserved_release("dev-1").is_some());
        assert!(reserved_release("detour").is_some());
        assert!(reserved_release("r1").is_none());
    }

    #[test]
    fn a_system_daemon_refuses_detours_and_a_user_daemon_needs_the_setting() {
        use crate::locations::Mode;

        let config = std::path::Path::new("/etc/muru/muru.toml");

        assert!(
            setting_refusal(true, Mode::System)
                .unwrap()
                .contains("system daemon")
        );
        assert!(setting_refusal(false, Mode::System).is_none());
        assert!(setting_refusal(true, Mode::User).is_none());

        assert!(
            opening_refusal(true, Mode::System, config)
                .unwrap()
                .contains("process owned by whoever ran")
        );
        assert!(
            opening_refusal(false, Mode::User, config)
                .unwrap()
                .contains("detours = true in the [daemon] table of /etc/muru/muru.toml")
        );
        assert!(opening_refusal(true, Mode::User, config).is_none());
    }

    #[test]
    fn a_secret_is_128_bits_and_never_repeats() {
        let one = secret().unwrap();
        let other = secret().unwrap();

        assert_eq!(one.len(), 32);
        assert!(one.chars().all(|digit| digit.is_ascii_hexdigit()));
        assert_ne!(one, other);
    }

    #[test]
    fn an_opening_needs_an_application_and_an_absolute_socket() {
        let request = pb::OpenDetourRequest {
            app: "shop".to_owned(),
            socket: "/run/user/1000/muru/run-1/detour.sock".to_owned(),
            ..Default::default()
        };

        assert!(Opening::from_message(request.clone()).is_ok());
        assert!(
            Opening::from_message(pb::OpenDetourRequest {
                socket: "detour.sock".to_owned(),
                ..request.clone()
            })
            .is_err()
        );
        assert!(
            Opening::from_message(pb::OpenDetourRequest {
                app: "Not An App".to_owned(),
                ..request
            })
            .is_err()
        );
    }
}
