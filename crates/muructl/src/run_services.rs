//! A project's services under `muructl serve` and `muructl run` (design
//! specification section 9.17): muructl is their supervisor, since there
//! is no daemon. Their output goes to standard output with every other
//! line, nothing of them is kept across runs, and under serve a service
//! may be required for the container to be ready.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use muru_controller::locations::HostDirs;
use muru_controller::services::{Begin, Service, Setting, Status};
use muru_controller::supervisor::{Report, Sink};

use crate::project_commands::Context;

/// How many of a service's last lines are kept for `muructl service logs`.
const KEPT_LINES: usize = 1000;

/// The quick exits in a row a required service gives up after, when its
/// own table does not say: the container is then replaced.
const REQUIRED_GIVE_UP_AFTER: u32 = 5;

/// Which of the project's services run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Every one that starts automatically.
    All,
    /// None.
    None,
    /// These, whether or not they start automatically.
    Named(Vec<String>),
}

impl Choice {
    /// The choice a setting's words make: `all`, `none`, or names; all
    /// when there are none.
    pub fn parse(words: &[String]) -> Self {
        match words {
            [] => Self::All,
            [one] if one == "all" => Self::All,
            [one] if one == "none" => Self::None,
            names => Self::Named(names.to_vec()),
        }
    }
}

/// The services the project declares, read from its file as it is now,
/// or from `file` in its place, and checked as a release's are.
pub fn declared(
    project: &Path,
    file: Option<&Path>,
) -> Result<BTreeMap<String, muru_config::Service>, String> {
    let Some((path, table)) =
        muru_config::load_settings(project, file).map_err(|error| error.to_string())?
    else {
        return Ok(BTreeMap::new());
    };

    muru_controller::config::resolve_services(&table.services)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// The declared services a choice runs, and an error for a name the
/// project does not declare.
pub fn chosen(
    declared: &BTreeMap<String, muru_config::Service>,
    choice: &Choice,
) -> Result<BTreeMap<String, muru_config::Service>, String> {
    match choice {
        Choice::All => Ok(declared.clone()),
        Choice::None => Ok(BTreeMap::new()),
        Choice::Named(names) => {
            if let Some(unknown) = names.iter().find(|name| !declared.contains_key(*name)) {
                return Err(format!(
                    "the project declares no service named {unknown}{}",
                    match declared.is_empty() {
                        true => String::new(),
                        false => format!(
                            ": it declares {}",
                            declared.keys().cloned().collect::<Vec<_>>().join(", ")
                        ),
                    }
                ));
            }

            Ok(declared
                .iter()
                .filter(|(name, _)| names.contains(name))
                .map(|(name, table)| (name.clone(), table.clone()))
                .collect())
        }
    }
}

/// The last lines of each service, by name.
type Lines = Arc<Mutex<BTreeMap<String, VecDeque<String>>>>;

/// The services of one run or serve.
#[derive(Default)]
pub struct RunServices {
    services: BTreeMap<String, Service>,
    lines: Lines,
    /// The directory the services' own directories are made under.
    run_dir: PathBuf,
    /// Whether a name given by the choice starts a service that does not
    /// start automatically.
    named: Vec<String>,
}

/// The services as the loop and the admin socket share them.
pub type Shared = Arc<tokio::sync::Mutex<RunServices>>;

impl RunServices {
    /// No services yet, with their directories to be made under
    /// `run_dir`.
    pub fn new(run_dir: &Path, choice: &Choice) -> Self {
        Self {
            run_dir: run_dir.join("s"),
            named: match choice {
                Choice::Named(names) => names.clone(),
                Choice::All | Choice::None => Vec::new(),
            },
            ..Self::default()
        }
    }

    /// What a service starts from: what a command of the project runs
    /// with, over muructl's own environment, in the project's directory,
    /// with `python` as its first word standing for the interpreter the
    /// application is served with.
    fn setting(
        &self,
        name: &str,
        table: &muru_config::Service,
        context: &Context,
    ) -> Result<Setting, String> {
        let (places, mut unset) = crate::places::variables(&context.places);
        unset.push(muru_controller::host::WORKER_ID_VARIABLE.to_owned());
        let mut base: Vec<(String, String)> = std::env::vars()
            .filter(|(variable, _)| !unset.contains(variable))
            .collect();
        base.extend(muru_controller::commands::environment(
            context.env.clone(),
            &muru_controller::commands::Interpreter {
                python: &context.python,
                runtime: context.pythonpath.as_deref(),
                pycache: None,
                bytecode: None,
                base: None,
                places: &places,
            },
            &context.app,
            &context.release,
            &context.project,
        ));

        // A secret is resolved by a daemon from its stores, and there is
        // none here: the value comes in the environment muructl has.

        let mut env = Vec::new();

        for (variable, value) in &table.env {
            match value {
                muru_config::EnvValue::Plain(value) => env.push((variable.clone(), value.clone())),
                muru_config::EnvValue::Secret(_) => {
                    return Err(format!(
                        "env.{variable} is a secret reference, which a daemon resolves; here \
                         the variable is given in the environment muructl is started with"
                    ));
                }
            }
        }

        let mut table = table.clone();
        table.command = muru_controller::commands::with_interpreter(table.command, &context.python);

        // A required service that never comes up has to end the
        // container, so it gives up where another would go on trying.

        if table.required == Some(true) {
            let restart = table.restart.get_or_insert_with(Default::default);
            restart.give_up_after.get_or_insert(REQUIRED_GIVE_UP_AFTER);
        }

        let cwd = match &table.cwd {
            Some(inside) => context.project.join(inside),
            None => context.project.clone(),
        };

        let lines = Arc::clone(&self.lines);
        let label = name.to_owned();
        let sink = Sink(Arc::new(move |index, _stderr, line: String| {
            let tagged = format!("muru.service.{label}[{index}]: {line}");
            muru_common::log::relayed(muru_common::log::Stream::Stdout, SystemTime::now(), &tagged);

            let mut kept = lines.lock().expect("service lines poisoned");
            let kept = kept.entry(label.clone()).or_default();
            kept.push_back(tagged);

            while kept.len() > KEPT_LINES {
                kept.pop_front();
            }
        }));

        Ok(Setting {
            name: name.to_owned(),
            table,
            dirs: HostDirs::together(&self.run_dir.join(name)),
            cwd,
            base,
            env,
            identity: None,
            log: PathBuf::new(),
            relays: None,
            owner: None,
            sink: Some(sink),
        })
    }

    /// Whether a service is started when it is begun: it starts
    /// automatically, or the choice named it.
    fn starts(&self, name: &str, table: &muru_config::Service) -> bool {
        muru_controller::services::autostart_of(table)
            || self.named.iter().any(|named| named == name)
    }

    /// Make the services those of `tables`, for the release `context` is
    /// of. One that already runs is replaced by one started from the
    /// new release's code and settings, the old instances stopped
    /// first; one that is new is begun; and one no longer declared is
    /// stopped. Gives what could not be done, by service.
    pub fn take(
        &mut self,
        tables: &BTreeMap<String, muru_config::Service>,
        context: &Context,
    ) -> Vec<(String, String)> {
        let mut failures = Vec::new();

        for (name, service) in &mut self.services {
            if !tables.contains_key(name) && !service.removing() {
                service.remove();
            }
        }

        for (name, table) in tables {
            let setting = match self.setting(name, table, context) {
                Ok(setting) => setting,
                Err(error) => {
                    failures.push((name.clone(), error));
                    continue;
                }
            };

            if let Some(service) = self.services.get_mut(name) {
                service.replace(setting, false);
                continue;
            }

            let begin = match self.starts(name, table) {
                true => Begin::Start,
                false => Begin::Idle,
            };

            match Service::begin(setting, false, begin) {
                Ok(service) => {
                    self.services.insert(name.clone(), service);
                }
                Err(error) => failures.push((name.clone(), error)),
            }
        }

        failures
    }

    /// Reap the instances that exited, start those that are due and
    /// take each one's readiness, with what happened, by service. A
    /// service that was removed is forgotten once nothing of it runs.
    pub async fn check(&mut self) -> Vec<(String, Report)> {
        let mut reports = Vec::new();

        for (name, service) in &mut self.services {
            reports.extend(
                service
                    .check()
                    .await
                    .into_iter()
                    .map(|report| (name.clone(), report)),
            );
        }

        self.services.retain(|_, service| !service.gone());
        reports
    }

    /// Each service as `muructl service list` gives it.
    pub fn statuses(&self) -> Vec<Status> {
        self.services
            .values()
            .filter(|service| !service.removing())
            .map(Service::status)
            .collect()
    }

    /// The services that are required, each with whether it is running
    /// and whether it has failed for good.
    pub fn required(&self) -> Vec<(String, bool, bool)> {
        self.statuses()
            .into_iter()
            .filter(|status| status.table.required == Some(true))
            .map(|status| {
                (
                    status.name.clone(),
                    status.state == "running",
                    status.state == "failed",
                )
            })
            .collect()
    }

    /// A service by name, to act on.
    pub fn service(&mut self, name: &str) -> Result<&mut Service, String> {
        self.services
            .get_mut(name)
            .filter(|service| !service.removing())
            .ok_or_else(|| format!("there is no service named {name}"))
    }

    /// The last `count` lines kept of a service; none for a service
    /// that is not there.
    pub fn lines(&self, name: &str, count: usize) -> Option<Vec<String>> {
        if !self.services.contains_key(name) {
            return None;
        }

        let kept = self.lines.lock().expect("service lines poisoned");
        let lines = kept.get(name).cloned().unwrap_or_default();
        Some(lines.into_iter().rev().take(count).rev().collect())
    }

    /// Stop every service, each with its own `stop_timeout` or what is
    /// left until `deadline`, whichever ends first.
    pub async fn stop_all(&mut self, deadline: std::time::Instant) {
        let stops = std::mem::take(&mut self.services)
            .into_values()
            .map(|service| {
                let grace = muru_controller::services::stop_timeout_of(service.table());
                service.stop_until(deadline.min(std::time::Instant::now() + grace))
            });
        futures::future::join_all(stops).await;
    }
}

/// How long the services have to stop in when nothing else bounds it.
pub const STOP_WITHIN: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(text: &str) -> BTreeMap<String, muru_config::Service> {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_choice_is_all_none_or_names_and_an_unknown_name_is_refused() {
        let words = |text: &[&str]| {
            text.iter()
                .map(|word| (*word).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(Choice::parse(&[]), Choice::All);
        assert_eq!(Choice::parse(&words(&["all"])), Choice::All);
        assert_eq!(Choice::parse(&words(&["none"])), Choice::None);
        assert_eq!(
            Choice::parse(&words(&["worker", "beat"])),
            Choice::Named(words(&["worker", "beat"]))
        );

        let declared = tables("[worker]\ncommand = ['w']\n[beat]\ncommand = ['b']\n");
        assert_eq!(chosen(&declared, &Choice::All).unwrap().len(), 2);
        assert!(chosen(&declared, &Choice::None).unwrap().is_empty());
        assert_eq!(
            chosen(&declared, &Choice::Named(words(&["beat"])))
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["beat"]
        );
        assert_eq!(
            chosen(&declared, &Choice::Named(words(&["mail"]))).unwrap_err(),
            "the project declares no service named mail: it declares beat, worker"
        );
    }
}
