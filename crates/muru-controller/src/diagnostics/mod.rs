//! What the diagnostic commands collect (design specification sections
//! 11.7 and 18.6): the facts about an installation that `muructl debug
//! report` summarises and `muructl debug bundle` archives.
//!
//! The collection is shared so that the two commands, and whatever else
//! comes to write the same archive, describe an installation in the same
//! terms. This module holds the facts themselves and the parts of the
//! collection that need no running daemon: what the machine says about
//! itself ([`facts`]) and what a daemon's state store holds ([`stored`]).
//! Asking a running daemon, a development run or `muructl serve` is the
//! command's own work, which fills the same types.

pub mod archive;
pub mod facts;
pub mod logs;
pub mod mask;
pub mod stored;
pub mod window;

use muru_proto::admin::v1 as pb;
use serde::Serialize;

/// A release as the diagnostics describe it, whether a running daemon
/// answered for it or its record was read from the state store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseFacts {
    /// The application.
    pub app: String,
    /// The release id.
    pub id: String,
    /// Its state: pending, ready, failed and so on.
    pub state: String,
    /// Why it failed, or what it is doing.
    pub message: String,
    /// Whether traffic goes to it.
    pub serving: bool,
    /// `workers`, or `http` for a managed HTTP server.
    pub kind: String,
    /// The application as `module:callable`; empty for a development
    /// run, which is asked for it with its settings.
    pub entrypoint: String,
    /// Its instrumentation modules (section 6.15), whose names the
    /// report replaces as it does the entry point's.
    pub instrumentation: Vec<String>,
    /// The interface as declared: auto, wsgi, asgi or grpc.
    pub interface: String,
    /// The interface its host settled on, empty until one is known.
    pub detected_interface: String,
    /// The frameworks a worker has loaded, each with its version, empty
    /// when no worker was asked.
    pub frameworks: Vec<String>,
    /// The interpreter's version, as `3.13.9`, empty when not known.
    pub python_version: String,
    /// `regular` or `free-threaded`, empty when not known.
    pub python_build: String,
    /// Its workers, as its host last reported them; empty when no host
    /// runs or none could be asked.
    pub workers: Vec<WorkerFacts>,
}

impl ReleaseFacts {
    /// The release as `app@id`.
    pub fn name(&self) -> String {
        format!("{}@{}", self.app, self.id)
    }

    /// The interface to show: what the host settled on, else as declared.
    pub fn interface_shown(&self) -> &str {
        if self.detected_interface.is_empty() {
            &self.interface
        } else {
            &self.detected_interface
        }
    }

    /// A release as a running daemon answers for it.
    pub fn from_message(release: &pb::Release) -> Self {
        // The tag names the build, as `cp313, free-threaded`.

        let python_build = if release.python_tag.is_empty() {
            ""
        } else if release.python_tag.contains("free-threaded") {
            "free-threaded"
        } else {
            "regular"
        };

        Self {
            app: release.app.clone(),
            id: release.id.clone(),
            state: release.state.clone(),
            message: release.message.clone(),
            serving: release.serving,
            kind: if release.kind.is_empty() {
                "workers".to_owned()
            } else {
                release.kind.clone()
            },
            entrypoint: release.entrypoint.clone(),
            instrumentation: release.instrumentation.clone(),
            interface: release.interface.clone(),
            detected_interface: release.detected_interface.clone(),
            frameworks: Vec::new(),
            python_version: release.python_version.clone(),
            python_build: python_build.to_owned(),
            workers: release
                .workers
                .iter()
                .map(|worker| WorkerFacts {
                    index: worker.index,
                    group: worker.group.clone(),
                    state: worker.state.clone(),
                })
                .collect(),
        }
    }
}

/// One worker of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkerFacts {
    /// Its index.
    pub index: u32,
    /// Its worker group.
    pub group: String,
    /// starting, ready, debugging, replacing or retiring.
    pub state: String,
}

/// One effective setting of a release, with the layer it came from, as
/// `muructl config show --effective` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Setting {
    /// What it is a setting of: `app@id`, or `app@id group NAME` and
    /// `app@id mount NAME` for a worker group's or a mount's own.
    pub scope: String,
    /// The key, as `timeouts.hard_timeout`.
    pub key: String,
    /// The value, as a table writes it.
    pub value: String,
    /// The layer the value came from.
    pub source: String,
}

impl Setting {
    /// A setting from a line of `config show --effective`, which reads
    /// `SCOPE: KEY = VALUE (SOURCE)`; none for a line of another shape,
    /// such as the one a release with no layers recorded has.
    pub fn parse(line: &str) -> Option<Self> {
        let (scope, rest) = line.split_once(": ")?;
        let (key, rest) = rest.split_once(" = ")?;
        let (value, source) = rest.strip_suffix(')')?.rsplit_once(" (")?;

        Some(Self {
            scope: scope.to_owned(),
            key: key.to_owned(),
            value: value.to_owned(),
            source: source.to_owned(),
        })
    }

    /// The release the setting belongs to, as `app@id`.
    pub fn release(&self) -> &str {
        self.scope.split(' ').next().unwrap_or_default()
    }
}

/// A part of the collection that could not be made, and why. Collection
/// carries on past one, so the result says what is missing in place of
/// what it would have held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotCollected {
    /// The part, by the name the output gives it.
    pub part: String,
    /// Why it could not be collected.
    pub why: String,
}

impl NotCollected {
    /// A part that could not be collected.
    pub fn new(part: &str, why: impl std::fmt::Display) -> Self {
        Self {
            part: part.to_owned(),
            why: why.to_string(),
        }
    }
}

/// The frameworks the diagnostics name, by the module a worker loads for
/// each. A worker's loaded modules are searched for these alone, so no
/// name of the application's own modules is reported.
const FRAMEWORKS: [&str; 16] = [
    "django",
    "flask",
    "fastapi",
    "starlette",
    "quart",
    "litestar",
    "falcon",
    "pyramid",
    "bottle",
    "tornado",
    "aiohttp",
    "sanic",
    "connexion",
    "werkzeug",
    "grpc",
    "channels",
];

/// The frameworks among a worker's loaded modules, as `debug inspect`
/// gives them, each with its version when the module states one.
pub fn frameworks(modules: &serde_json::Value) -> Vec<String> {
    FRAMEWORKS
        .iter()
        .filter_map(|name| {
            let module = modules.get(name)?;

            Some(match module["version"].as_str() {
                Some(version) => format!("{name} {version}"),
                None => (*name).to_owned(),
            })
        })
        .collect()
}

/// The events that change the configuration in force, by type, a type
/// ending in a full stop standing for every type that starts with it.
const CONFIGURATION_EVENTS: [&str; 16] = [
    "config.applied",
    "config.reloaded",
    "traffic.",
    "traffic_policy.",
    "route.",
    "maintenance.changed",
    "access_log.",
    "release.created",
    "release.removed",
    "rollout.",
    "overlay.activated",
    "overlay.ended",
    "overlay.promoted",
    "schedule.",
    "client_class.list_reloaded",
    "deploy.freeze_overridden",
];

/// Whether an event of this type is a change to the configuration in
/// force: an apply or a reload, traffic moved, a deploy, a rollout's
/// step, an overlay or a schedule starting or ending, and the like.
pub fn changes_configuration(kind: &str) -> bool {
    CONFIGURATION_EVENTS.iter().any(|known| {
        if known.ends_with('.') {
            kind.starts_with(known)
        } else {
            kind == *known
        }
    })
}

/// The events that record something going wrong, by type, a type ending
/// in a full stop standing for every type that starts with it: a request
/// past a timeout, a worker killed, recycled or gone, a release or a
/// server that failed.
const FAILURE_EVENTS: [&str; 18] = [
    "request.soft_timeout",
    "request.past_deadline",
    "worker.timeout_killed",
    "worker.oom_killed",
    "worker.recycled",
    "worker.exited",
    "worker.loop_stalled",
    "worker.gil_stalled",
    "worker.restarted",
    "release.failed",
    "release.import_failed",
    "release.build_failed",
    "host.exited",
    "server.exited",
    "server.failed",
    "edge.failed",
    "preload.fell_back",
    "memory.leak_suspected",
];

/// Whether an event of this type records a failure: what the automatic
/// captures of section 11.7 are attached to, and what replaced or lost a
/// worker.
pub fn is_failure(kind: &str) -> bool {
    FAILURE_EVENTS.contains(&kind)
}

/// The distributions installed in a `site-packages` directory, each as
/// `name version`, from the names of their `.dist-info` directories.
/// Nothing is run and no file is opened.
pub fn installed(site_packages: &std::path::Path) -> std::io::Result<Vec<String>> {
    let mut found = Vec::new();

    for entry in std::fs::read_dir(site_packages)? {
        let name = entry?.file_name();

        if let Some(distribution) = name.to_string_lossy().strip_suffix(".dist-info") {
            found.push(match distribution.rsplit_once('-') {
                Some((name, version)) => format!("{name} {version}"),
                None => distribution.to_owned(),
            });
        }
    }

    found.sort();
    Ok(found)
}

/// The `site-packages` directories of the environment an interpreter is
/// in: those under the `lib` beside its `bin`.
pub fn site_packages(python: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Some(lib) = python
        .parent()
        .and_then(std::path::Path::parent)
        .map(|root| root.join("lib"))
    else {
        return Vec::new();
    };

    let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(lib)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("site-packages"))
        .filter(|path| path.is_dir())
        .collect();
    found.sort();
    found
}

/// The event that records a bundle being made: what it is about (the
/// application or release the bundle is limited to, else the daemon) and
/// its sentence, which says how far back the bundle reaches.
pub fn bundle_event(request: &pb::NoteBundleRequest) -> (String, String) {
    let subject = match (request.app.as_str(), request.release.as_str()) {
        ("", _) => "daemon".to_owned(),
        (app, "") => app.to_owned(),
        (app, release) => format!("{app}@{release}"),
    };

    let of = if request.app.is_empty() {
        "the whole node".to_owned()
    } else {
        subject.clone()
    };
    let reach = period(request.until_unix_ms.saturating_sub(request.since_unix_ms) / 1000);

    let with = if request.options.is_empty() {
        String::new()
    } else {
        format!(", with {}", request.options.join(", "))
    };

    (
        subject,
        format!("a diagnostic bundle of {of} was made, reaching back {reach}{with}"),
    )
}

/// A period of whole seconds in the largest unit that divides it, as a
/// duration is written on a command line: `1h`, `90m`, `45s`.
pub fn period(seconds: u64) -> String {
    for (unit, length) in [("d", 86_400), ("h", 3600), ("m", 60)] {
        if seconds != 0 && seconds.is_multiple_of(length) {
            return format!("{}{unit}", seconds / length);
        }
    }

    format!("{seconds}s")
}

/// An event of the store as the admin API gives it.
pub fn event_message(event: crate::model::Event) -> pb::Event {
    pb::Event {
        sequence: event.sequence,
        time_unix_ms: event.time_unix_ms,
        r#type: event.kind,
        subject: event.subject,
        level: event.level.to_owned(),
        message: event.message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_setting_is_read_from_a_line_of_the_effective_configuration() {
        let setting = Setting::parse("shop@r3: processes = 2 (the deploy command)").unwrap();
        assert_eq!(setting.scope, "shop@r3");
        assert_eq!(setting.key, "processes");
        assert_eq!(setting.value, "2");
        assert_eq!(setting.source, "the deploy command");
        assert_eq!(setting.release(), "shop@r3");

        // A value may itself hold a bracket, and a scope a group.

        let setting = Setting::parse(
            "shop@r3 group reports: timeouts.queue_timeout = 5s (x) (the worker group)",
        )
        .unwrap();
        assert_eq!(setting.scope, "shop@r3 group reports");
        assert_eq!(setting.value, "5s (x)");
        assert_eq!(setting.source, "the worker group");
        assert_eq!(setting.release(), "shop@r3");

        assert_eq!(
            Setting::parse("shop@r1: made before Muru recorded where settings come from"),
            None
        );
    }

    #[test]
    fn the_event_for_a_bundle_says_what_it_was_made_of() {
        let whole = pb::NoteBundleRequest {
            since_unix_ms: 1_000_000,
            until_unix_ms: 1_000_000 + 3_600_000,
            ..pb::NoteBundleRequest::default()
        };
        assert_eq!(
            bundle_event(&whole),
            (
                "daemon".to_owned(),
                "a diagnostic bundle of the whole node was made, reaching back 1h".to_owned()
            )
        );

        let narrowed = pb::NoteBundleRequest {
            app: "shop".to_owned(),
            release: "r3".to_owned(),
            since_unix_ms: 1_000_000,
            until_unix_ms: 1_000_000 + 90_000,
            options: vec!["scrub".to_owned(), "memory".to_owned()],
        };
        assert_eq!(period(5400), "90m");
        assert_eq!(period(172_800), "2d");
        assert_eq!(period(0), "0s");
        assert_eq!(
            bundle_event(&narrowed),
            (
                "shop@r3".to_owned(),
                "a diagnostic bundle of shop@r3 was made, reaching back 90s, with scrub, memory"
                    .to_owned()
            )
        );
    }

    #[test]
    fn the_events_that_change_configuration_are_told_from_the_rest() {
        for kind in [
            "config.reloaded",
            "traffic.switched",
            "release.created",
            "rollout.step",
            "overlay.ended",
            "access_log.reverted",
        ] {
            assert!(changes_configuration(kind), "{kind}");
        }

        for kind in [
            "release.ready",
            "worker.exited",
            "config.apply_failed",
            "overlay.skipped",
            "debug.bundle",
        ] {
            assert!(!changes_configuration(kind), "{kind}");
        }
    }

    #[test]
    fn the_distributions_of_an_environment_are_read_from_its_directories() {
        let root = tempfile::tempdir().unwrap();
        let python = root.path().join("bin/python");
        let site = root.path().join("lib/python3.13/site-packages");
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();

        for directory in [
            "flask-3.0.3.dist-info",
            "Werkzeug-3.0.1.dist-info",
            "typing_extensions-4.12.2.dist-info",
            "flask",
            "__pycache__",
        ] {
            std::fs::create_dir_all(site.join(directory)).unwrap();
        }

        assert_eq!(site_packages(&python), std::slice::from_ref(&site));
        assert_eq!(
            installed(&site).unwrap(),
            ["Werkzeug 3.0.1", "flask 3.0.3", "typing_extensions 4.12.2"]
        );
        assert!(site_packages(std::path::Path::new("/no/such/bin/python")).is_empty());
        assert!(installed(&root.path().join("nowhere")).is_err());

        assert!(is_failure("worker.timeout_killed"));
        assert!(is_failure("request.soft_timeout"));
        assert!(!is_failure("worker.ready"));
    }

    #[test]
    fn only_known_frameworks_are_named_from_a_workers_modules() {
        let modules = serde_json::json!({
            "acme_billing": {"version": "9.9", "file": "/srv/acme/billing.py"},
            "flask": {"version": "3.0.3", "file": "/venv/flask/__init__.py"},
            "werkzeug": {"version": null, "file": "/venv/werkzeug/__init__.py"},
        });

        assert_eq!(frameworks(&modules), ["flask 3.0.3", "werkzeug"]);
    }

    #[test]
    fn a_release_a_daemon_answers_for_names_its_build() {
        let release = pb::Release {
            app: "shop".to_owned(),
            id: "r3".to_owned(),
            state: "ready".to_owned(),
            interface: "auto".to_owned(),
            detected_interface: "wsgi".to_owned(),
            python_version: "3.14.0".to_owned(),
            python_tag: "cp314t, free-threaded".to_owned(),
            serving: true,
            ..pb::Release::default()
        };
        let facts = ReleaseFacts::from_message(&release);

        assert_eq!(facts.name(), "shop@r3");
        assert_eq!(facts.kind, "workers");
        assert_eq!(facts.interface_shown(), "wsgi");
        assert_eq!(facts.python_build, "free-threaded");
    }
}
