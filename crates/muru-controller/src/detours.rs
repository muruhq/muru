//! Detours (design specification section 9.13, ADR 0075): a development
//! run that a daemon routes to as a release of kind `detour` of an
//! application, for as long as the run holds its admin call open. The
//! daemon keeps them in memory alone; this module has what it keeps and
//! the choices that need no daemon.

use std::collections::BTreeMap;
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

/// How long an entry link to a detour's preview lasts (section 4.21).
pub const ENTRY_TTL: std::time::Duration = std::time::Duration::from_secs(60);

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
    /// When its traffic goes back to preview on its own, Unix
    /// milliseconds; 0 for never.
    pub until_unix_ms: u64,
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
            until_unix_ms: self.until_unix_ms,
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

/// The name of the runtime overlay a detour's traffic is carried by,
/// one for each application (section 9.13).
pub fn overlay_name(app: &str) -> String {
    format!("detour:{app}")
}

/// Whether an overlay's name is one a detour owns, which the overlay
/// commands leave alone.
pub fn is_overlay(name: &str) -> bool {
    name.starts_with("detour:")
}

/// What one detour asks of its application's routes.
#[derive(Debug, Clone, Copy)]
pub struct Taking<'a> {
    /// The application.
    pub app: &'a str,
    /// The detour's release.
    pub release: &'a str,
    /// What it takes.
    pub traffic: &'a Traffic,
}

/// Lay the detours' traffic over a route, when it is their
/// application's: `all` pins it to the detour, `share=N` replaces its
/// split with N percent to the detour, the serving release taking the
/// rest, and `class=NAME` gives the class the detour. A route of
/// another application, or detours that take previews alone, leave it
/// as it is.
pub fn lay_over(route: &mut muru_config::Route, takings: &[Taking<'_>]) {
    let own: Vec<&Taking<'_>> = takings
        .iter()
        .filter(|taking| taking.app == route.app && taking.traffic != &Traffic::Preview)
        .collect();

    if own.is_empty() {
        return;
    }

    let traffic = route.traffic.get_or_insert_with(Default::default);
    let mut shares = BTreeMap::new();

    for taking in &own {
        match taking.traffic {
            Traffic::Preview => {}
            Traffic::All => {
                traffic.split = BTreeMap::from([(taking.release.to_owned(), 100)]);
                shares.clear();
                break;
            }
            Traffic::Share(share) => {
                shares.insert(taking.release.to_owned(), *share);
            }
            Traffic::Class(class) => {
                traffic
                    .classes
                    .insert(class.clone(), taking.release.to_owned());
            }
        }
    }

    if !shares.is_empty() {
        traffic.split = shares;
    }
}

/// What the overlay of an application's detours changes, for `overlay
/// list`: one line for each detour that takes more than previews.
pub fn changes(takings: &[Taking<'_>]) -> Vec<String> {
    takings
        .iter()
        .filter(|taking| taking.traffic != &Traffic::Preview)
        .map(|taking| {
            let what = match taking.traffic {
                Traffic::Preview => String::new(),
                Traffic::All => "every route pinned".to_owned(),
                Traffic::Share(share) => format!("{share}% of every route"),
                Traffic::Class(class) => format!("client class {class} on every route"),
            };

            format!("app {} to {}: {what}", taking.app, taking.release)
        })
        .collect()
}

/// A detour's traffic as listings show it: the choice, and when it goes
/// back to preview, as HH:MM UTC, when it is timed.
pub fn traffic_text(traffic: &str, until_unix_ms: u64) -> String {
    if until_unix_ms == 0 {
        traffic.to_owned()
    } else {
        format!("{traffic} until {}", crate::daemon::clock(until_unix_ms))
    }
}

/// Why `traffic` cannot end on its own at `until_unix_ms` (0 for never),
/// at `now_unix_ms`: preview is what a timed choice goes back to, so it
/// has nothing to end, and the end must be still to come.
pub fn timed_refusal(traffic: &Traffic, until_unix_ms: u64, now_unix_ms: u64) -> Option<String> {
    if until_unix_ms == 0 {
        return None;
    }

    if *traffic == Traffic::Preview {
        return Some(
            "a detour goes back to preview when a timed choice ends, so preview takes no end \
             time"
                .to_owned(),
        );
    }

    (until_unix_ms <= now_unix_ms)
        .then(|| "the time given for the detour's traffic to end has passed already".to_owned())
}

/// Why a detour cannot take `wanted` of its application, given the
/// application's other detours, whether a rollout of it is under way
/// and the client classes the daemon defines (section 9.13).
pub fn traffic_refusal(
    taking: Taking<'_>,
    others: &[Taking<'_>],
    rollout: bool,
    classes: &[String],
) -> Option<String> {
    let app = taking.app;

    match taking.traffic {
        Traffic::Preview => None,
        Traffic::All | Traffic::Share(_) if rollout => Some(format!(
            "a rollout of {app} is under way, and {} would take traffic from it; wait for it to \
             end, or abort it",
            taking.traffic.word()
        )),
        Traffic::All => others
            .iter()
            .find(|other| other.traffic == &Traffic::All)
            .map(|other| {
                format!(
                    "detour {app}@{} takes all of {app}'s traffic already; set its traffic to \
                     preview first",
                    other.release
                )
            }),
        Traffic::Share(share) => {
            let taken: u32 = others
                .iter()
                .map(|other| match other.traffic {
                    Traffic::Share(share) => *share,
                    Traffic::All => 100,
                    _ => 0,
                })
                .sum();

            (taken + share > 100).then(|| {
                format!(
                    "{app}'s other detours take {taken}% already, so share={share} would make \
                     more than the whole"
                )
            })
        }
        Traffic::Class(class) if !classes.contains(class) => Some(format!(
            "the daemon defines no client class {class}{}",
            if classes.is_empty() {
                String::new()
            } else {
                format!("; it has {}", classes.join(", "))
            }
        )),
        Traffic::Class(_) => None,
    }
}

/// The detour `wanted` names among `detours` (section 13.12), by the
/// instance resolver's rules over the detours' runs, with two of a
/// detour's own first: its release, and its application when one detour
/// has it. With no value, the one detour there is. An ambiguous value is
/// refused, listing each detour it matched with the value to pass for it.
pub fn choose<'a>(
    wanted: Option<&str>,
    detours: &'a [pb::Detour],
) -> Result<&'a pb::Detour, String> {
    let listing = "muructl detour list";

    let Some(wanted) = wanted.map(str::trim).filter(|wanted| !wanted.is_empty()) else {
        return match detours {
            [] => Err("no detour is open on this daemon".to_owned()),
            [only] => Ok(only),
            several => Err(ambiguous("", several, detours)),
        };
    };

    if let Some(found) = detours.iter().find(|detour| detour.release == wanted) {
        return Ok(found);
    }

    let of_app: Vec<&pb::Detour> = detours
        .iter()
        .filter(|detour| detour.app == wanted)
        .collect();

    match of_app.as_slice() {
        [only] => return Ok(only),
        [] => {}
        several => {
            let several: Vec<pb::Detour> = several.iter().map(|detour| (*detour).clone()).collect();
            return Err(ambiguous(wanted, &several, detours));
        }
    }

    // The rest are the resolver's: the run's tag, a prefix of its ID and
    // its label, over records made from what the daemon was told.

    let records: Vec<crate::registry::Record> = detours.iter().map(record_of).collect();

    match crate::selector::select(wanted, &records) {
        Ok(choice) => detours
            .iter()
            .find(|detour| detour.instance_id == choice.record.id.to_string())
            .ok_or_else(|| format!("{wanted} names no open detour; {listing} shows them")),
        Err(crate::selector::Refusal::Ambiguous { .. }) => {
            let matched: Vec<pb::Detour> = detours
                .iter()
                .filter(|detour| detour.label == wanted)
                .cloned()
                .collect();
            Err(ambiguous(wanted, &matched, detours))
        }
        Err(_) => Err(format!(
            "{wanted} is not the release, application, tag, ID or label of an open detour; \
             {listing} shows them"
        )),
    }
}

/// A detour's run as a registry record, for the instance resolver.
fn record_of(detour: &pb::Detour) -> crate::registry::Record {
    let id = detour
        .instance_id
        .parse()
        .unwrap_or_else(|_| crate::registry::InstanceId::nil());
    let mut record = crate::registry::Record::new(
        id,
        crate::registry::Kind::Detour,
        crate::client::Endpoint::unix(&detour.socket),
        PathBuf::from(&detour.directory),
    );
    record.label = (!detour.label.is_empty()).then(|| detour.label.clone());
    record.app = Some(detour.app.clone());
    record.project = Some(PathBuf::from(&detour.directory));
    record.pid = detour.pid;
    record
}

/// The value that chooses `detour` alone among `detours`: its label when
/// no other has it, else its release.
pub fn handle(detour: &pb::Detour, detours: &[pb::Detour]) -> String {
    let label_alone = !detour.label.is_empty()
        && detours
            .iter()
            .filter(|other| other.label == detour.label)
            .count()
            == 1;

    if label_alone {
        detour.label.clone()
    } else {
        detour.release.clone()
    }
}

/// The refusal of a value several detours match.
fn ambiguous(wanted: &str, matched: &[pb::Detour], detours: &[pb::Detour]) -> String {
    let lines: Vec<(String, String)> = matched
        .iter()
        .map(|detour| {
            (
                handle(detour, detours),
                format!(
                    "{}@{}, pid {} in {}",
                    detour.app,
                    detour.release,
                    detour.pid,
                    crate::registry::home_shortened(std::path::Path::new(&detour.directory))
                ),
            )
        })
        .collect();
    let width = lines
        .iter()
        .map(|(value, _)| value.len())
        .max()
        .unwrap_or(0);

    let mut text = if wanted.is_empty() {
        format!("{} detours are open; name one:", matched.len())
    } else {
        format!("{wanted} names {} open detours; name one:", matched.len())
    };

    for (value, what) in lines.iter().take(crate::selector::MAX_LISTED) {
        text.push_str(&format!("\n  {value:width$}  {what}"));
    }

    if lines.len() > crate::selector::MAX_LISTED {
        text.push_str(&format!(
            "\n  and {} more; muructl detour list shows them all",
            lines.len() - crate::selector::MAX_LISTED
        ));
    }

    text
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
    fn a_timed_choice_needs_something_to_end_and_an_end_to_come() {
        let now = 1_800_000_000_000;

        // Untimed, anything goes; timed, preview has nothing to end and
        // the end must be ahead.

        assert_eq!(timed_refusal(&Traffic::Preview, 0, now), None);
        assert_eq!(timed_refusal(&Traffic::Share(10), now + 60_000, now), None);
        assert!(
            timed_refusal(&Traffic::Preview, now + 60_000, now)
                .unwrap()
                .contains("preview takes no end time")
        );
        assert!(
            timed_refusal(&Traffic::All, now, now)
                .unwrap()
                .contains("has passed already")
        );

        // Listings give the end as a UTC clock.

        assert_eq!(traffic_text("all", 0), "all");
        assert_eq!(
            traffic_text("share=10", 1_800_000_000_000 + 25 * 60_000),
            "share=10 until 08:25 UTC"
        );
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

    fn route(app: &str) -> muru_config::Route {
        muru_config::Route {
            app: app.to_owned(),
            ..muru_config::Route::default()
        }
    }

    /// What a detour of shop asks.
    fn taking<'a>(release: &'a str, traffic: &'a Traffic) -> Taking<'a> {
        Taking {
            app: "shop",
            release,
            traffic,
        }
    }

    #[test]
    fn each_choice_is_laid_over_the_routes_of_its_application_alone() {
        let all = &Traffic::All;
        let share = &Traffic::Share(10);
        let class = &Traffic::Class("crawlers".to_owned());
        let preview = &Traffic::Preview;

        let mut pinned = route("shop");
        lay_over(&mut pinned, &[taking("dev-1", all)]);
        assert_eq!(
            pinned.traffic.unwrap().split,
            BTreeMap::from([("dev-1".to_owned(), 100)])
        );

        let mut split = route("shop");
        split.traffic = Some(muru_config::Traffic {
            split: BTreeMap::from([("r2".to_owned(), 50)]),
            sticky: Some(true),
            ..Default::default()
        });
        lay_over(
            &mut split,
            &[taking("dev-1", share), taking("dev-2", preview)],
        );
        let traffic = split.traffic.unwrap();
        assert_eq!(traffic.split, BTreeMap::from([("dev-1".to_owned(), 10)]));
        assert_eq!(traffic.sticky, Some(true), "the rest of the policy stays");

        let mut classed = route("shop");
        lay_over(&mut classed, &[taking("dev-1", class)]);
        assert_eq!(
            classed.traffic.unwrap().classes,
            BTreeMap::from([("crawlers".to_owned(), "dev-1".to_owned())])
        );

        let mut other = route("blog");
        lay_over(&mut other, &[taking("dev-1", all)]);
        assert!(other.traffic.is_none());

        let mut previewed = route("shop");
        lay_over(&mut previewed, &[taking("dev-1", preview)]);
        assert!(previewed.traffic.is_none());

        assert_eq!(
            changes(&[taking("dev-1", all), taking("dev-2", preview)]),
            ["app shop to dev-1: every route pinned"]
        );
    }

    #[test]
    fn all_and_a_share_wait_for_a_rollout_and_never_take_more_than_the_whole() {
        let all = Traffic::All;
        let share = Traffic::Share(60);
        let crawlers = Traffic::Class("crawlers".to_owned());
        let bots = Traffic::Class("bots".to_owned());
        let preview = Traffic::Preview;
        let classes = ["crawlers".to_owned()];

        assert!(traffic_refusal(taking("dev-1", &all), &[], false, &classes).is_none());
        assert!(
            traffic_refusal(taking("dev-1", &all), &[], true, &classes)
                .unwrap()
                .contains("rollout")
        );
        assert!(
            traffic_refusal(taking("dev-1", &share), &[], true, &classes)
                .unwrap()
                .contains("rollout")
        );
        assert!(traffic_refusal(taking("dev-1", &preview), &[], true, &classes).is_none());
        assert!(traffic_refusal(taking("dev-1", &crawlers), &[], true, &classes).is_none());

        assert!(
            traffic_refusal(
                taking("dev-2", &all),
                &[taking("dev-1", &all)],
                false,
                &classes
            )
            .unwrap()
            .contains("dev-1 takes all")
        );
        assert!(
            traffic_refusal(
                taking("dev-2", &share),
                &[taking("dev-1", &share)],
                false,
                &classes
            )
            .unwrap()
            .contains("more than the whole")
        );
        assert!(
            traffic_refusal(taking("dev-1", &bots), &[], false, &classes)
                .unwrap()
                .contains("no client class bots; it has crawlers")
        );
    }

    fn detour(release: &str, app: &str, label: &str, id: &str) -> pb::Detour {
        pb::Detour {
            app: app.to_owned(),
            release: release.to_owned(),
            label: label.to_owned(),
            instance_id: id.to_owned(),
            pid: 4123,
            directory: "/home/me/shop".to_owned(),
            socket: "/run/user/1000/muru/run-1/detour.sock".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn a_detour_is_chosen_by_release_application_tag_id_or_label() {
        let one = detour(
            "dev-7f3a",
            "shop",
            "main",
            "7f3a91c2d4e5f60718293a4b5c6d7e8f",
        );
        let two = detour(
            "dev-1b2c",
            "shop",
            "feature-x",
            "1b2c3d4e5f60718293a4b5c6d7e8f901",
        );
        let blog = detour("dev-9999", "blog", "", "99990000aaaabbbbccccddddeeeeffff");
        let detours = [one.clone(), two.clone(), blog.clone()];

        assert_eq!(choose(Some("dev-1b2c"), &detours).unwrap(), &two);
        assert_eq!(choose(Some("blog"), &detours).unwrap(), &blog);
        assert_eq!(choose(Some("detour:7f3a91c2"), &detours).unwrap(), &one);
        assert_eq!(choose(Some("1b2c3d"), &detours).unwrap(), &two);
        assert_eq!(choose(Some("feature-x"), &detours).unwrap(), &two);

        let refused = choose(Some("shop"), &detours).unwrap_err();
        assert!(refused.contains("shop names 2 open detours"), "{refused}");
        assert!(
            refused.contains("main") && refused.contains("feature-x"),
            "{refused}"
        );

        assert!(choose(None, &detours).is_err());
        assert_eq!(choose(None, std::slice::from_ref(&blog)).unwrap(), &blog);
        assert!(choose(Some("nothing"), &detours).is_err());
        assert!(choose(None, &[]).unwrap_err().contains("no detour is open"));
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
