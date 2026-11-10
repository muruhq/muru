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

pub mod facts;
pub mod stored;

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
