//! An application's own release settings (issue 899, ADR 0076, section
//! 3.3's layer 4): the environment, secrets by reference, worker counts,
//! recycling, the host's timeouts and the memory policy that every release
//! deployed after they are set takes, over the source's `[tool.muru]` and
//! under the release's own flags or table. They are kept with the
//! application, set by `muructl app set` (the SetAppSettings call) or by
//! `[app.NAME]` keys in an applied document, and recorded in each release
//! as the layer `the application`.
//!
//! Tables (groups, mounts, static entries, logging, scaling, error pages)
//! stay with the source and the release, and so do the settings that say
//! what the code is: the entrypoint, the interface and the interpreter.

use std::collections::BTreeMap;
use std::time::Duration;

use muru_proto::admin::v1 as pb;
use serde::{Deserialize, Serialize};

use crate::model::EnvSpec;

/// The layer a release records for a setting its application gave.
pub const LAYER: &str = "the application";

/// What the settings are, every one optional; the environment by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    /// The variables, by name, secrets by reference.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvSpec>,
    /// Worker processes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<u32>,
    /// Application threads per worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<u32>,
    /// Threads for synchronous code called from ASGI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_threads: Option<u32>,
    /// Requests an ASGI worker's loop takes at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    /// Requests a worker serves before it is replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<u64>,
    /// How long a worker lives before it is replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lifetime_ms: Option<u64>,
    /// Workers a group may run over its count while workers are swapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_surge: Option<u32>,
    /// Workers a group may run under its count while workers are swapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_unavailable: Option<u32>,
    /// The host's timeouts, by the key of a `timeouts` table.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub timeouts_ms: BTreeMap<String, u64>,
    /// What memory is measured as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_measure: Option<muru_config::Measure>,
    /// The soft memory limit, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_soft_limit: Option<u64>,
    /// How long a worker may be over the soft limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_soft_for_ms: Option<u64>,
    /// Whether a worker over the soft limit reclaims first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_reclaim_first: Option<bool>,
    /// The hard memory limit, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_hard_limit: Option<u64>,
    /// How often memory is measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_check_interval_ms: Option<u64>,
    /// What the node changes of the services the releases declare
    /// (section 9.17, issue 1011), by the service's name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, muru_config::ServiceOverride>,
}

/// The host timeouts an application sets, by their key in a `timeouts`
/// table, in the order they are shown.
pub const TIMEOUTS: [&str; 8] = [
    "soft_timeout",
    "hard_timeout",
    "hard_drain_grace",
    "shutdown_timeout",
    "websocket_drain_grace",
    "exit_grace",
    "loop_stall_timeout",
    "gil_stall_timeout",
];

/// The keys `unset` takes besides `timeouts.KEY` and `env.NAME`.
const SCALARS: [&str; 14] = [
    "processes",
    "threads",
    "sync_threads",
    "concurrency",
    "max_requests",
    "max_lifetime",
    "max_surge",
    "max_unavailable",
    "memory.measure",
    "memory.soft_limit",
    "memory.soft_for",
    "memory.reclaim_first",
    "memory.hard_limit",
    "memory.check_interval",
];

/// A duration in milliseconds as a table writes it.
fn duration(ms: u64) -> String {
    muru_config::format_duration(Duration::from_millis(ms))
}

/// A duration a table gives, in milliseconds.
fn parse_ms(text: &str, key: &str) -> Result<u64, String> {
    muru_config::parse_duration(text)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| format!("{key}: {error}"))
}

/// A size a table gives, in bytes.
fn parse_bytes(text: &str, key: &str) -> Result<u64, String> {
    muru_config::parse_size(text).map_err(|error| format!("{key}: {error}"))
}

impl AppSettings {
    /// Whether nothing is set.
    pub fn is_empty(&self) -> bool {
        *self == AppSettings::default()
    }

    /// The settings an admin message gives, checked.
    pub fn from_message(message: &pb::AppSettings) -> Result<AppSettings, String> {
        let mut env = BTreeMap::new();

        for entry in &message.env {
            muru_config::check_env_name(&entry.name)?;

            if entry.secret_store.is_empty() != entry.secret_key.is_empty() {
                return Err(format!(
                    "env {}: a secret needs both a store and a key",
                    entry.name
                ));
            }

            env.insert(
                entry.name.clone(),
                EnvSpec {
                    name: entry.name.clone(),
                    value: entry.secret_store.is_empty().then(|| entry.value.clone()),
                    secret_store: (!entry.secret_store.is_empty())
                        .then(|| entry.secret_store.clone()),
                    secret_key: (!entry.secret_key.is_empty()).then(|| entry.secret_key.clone()),
                    as_file: entry.as_file && !entry.secret_store.is_empty(),
                },
            );
        }

        let timeouts_ms = TIMEOUTS
            .iter()
            .zip([
                message.soft_timeout_ms,
                message.hard_timeout_ms,
                message.hard_drain_grace_ms,
                message.shutdown_timeout_ms,
                message.websocket_drain_grace_ms,
                message.exit_grace_ms,
                message.loop_stall_timeout_ms,
                message.gil_stall_timeout_ms,
            ])
            .filter_map(|(key, ms)| Some(((*key).to_owned(), ms?)))
            .collect();

        let memory_measure = match message.memory_measure.as_str() {
            "" => None,
            text => Some(
                text.parse()
                    .map_err(|error: muru_config::ParseError| format!("memory.measure: {error}"))?,
            ),
        };

        let settings = AppSettings {
            env: env.into_values().collect(),
            processes: message.processes,
            threads: message.threads,
            sync_threads: message.sync_threads,
            concurrency: message.concurrency,
            max_requests: message.max_requests,
            max_lifetime_ms: message.max_lifetime_ms,
            max_surge: message.max_surge,
            max_unavailable: message.max_unavailable,
            timeouts_ms,
            memory_measure,
            memory_soft_limit: message.memory_soft_limit,
            memory_soft_for_ms: message.memory_soft_for_ms,
            memory_reclaim_first: message.memory_reclaim_first,
            memory_hard_limit: message.memory_hard_limit,
            memory_check_interval_ms: message.memory_check_interval_ms,
            services: message
                .services
                .iter()
                .map(|(name, text)| {
                    serde_json::from_str::<muru_config::ServiceOverride>(text)
                        .map(|settings| (name.clone(), settings))
                        .map_err(|error| format!("service.{name}: {error}"))
                })
                .collect::<Result<_, _>>()?,
        };

        settings.check()?;
        Ok(settings)
    }

    /// The settings as an admin message.
    pub fn to_message(&self) -> pb::AppSettings {
        let timeout = |key: &str| self.timeouts_ms.get(key).copied();

        pb::AppSettings {
            env: self
                .env
                .iter()
                .map(|entry| pb::EnvEntry {
                    name: entry.name.clone(),
                    value: entry.value.clone().unwrap_or_default(),
                    secret_store: entry.secret_store.clone().unwrap_or_default(),
                    secret_key: entry.secret_key.clone().unwrap_or_default(),
                    as_file: entry.as_file,
                })
                .collect(),
            processes: self.processes,
            threads: self.threads,
            sync_threads: self.sync_threads,
            concurrency: self.concurrency,
            max_requests: self.max_requests,
            max_lifetime_ms: self.max_lifetime_ms,
            max_surge: self.max_surge,
            max_unavailable: self.max_unavailable,
            soft_timeout_ms: timeout("soft_timeout"),
            hard_timeout_ms: timeout("hard_timeout"),
            hard_drain_grace_ms: timeout("hard_drain_grace"),
            shutdown_timeout_ms: timeout("shutdown_timeout"),
            websocket_drain_grace_ms: timeout("websocket_drain_grace"),
            exit_grace_ms: timeout("exit_grace"),
            loop_stall_timeout_ms: timeout("loop_stall_timeout"),
            gil_stall_timeout_ms: timeout("gil_stall_timeout"),
            memory_measure: self
                .memory_measure
                .map(|measure| measure.as_str().to_owned())
                .unwrap_or_default(),
            memory_soft_limit: self.memory_soft_limit,
            memory_soft_for_ms: self.memory_soft_for_ms,
            memory_reclaim_first: self.memory_reclaim_first,
            memory_hard_limit: self.memory_hard_limit,
            memory_check_interval_ms: self.memory_check_interval_ms,
            services: self
                .services
                .iter()
                .map(|(name, settings)| {
                    (
                        name.clone(),
                        serde_json::to_string(settings).unwrap_or_default(),
                    )
                })
                .collect(),
            notice: String::new(),
        }
    }

    /// The settings an `[app.NAME]` table gives, checked.
    pub fn from_table(table: &muru_config::App) -> Result<AppSettings, String> {
        let mut env = Vec::new();

        for (name, value) in &table.env {
            env.push(crate::config::env_spec_from_table(name, value)?);
        }

        let count = |value: Option<usize>, key: &str| -> Result<Option<u32>, String> {
            value
                .map(|value| {
                    u32::try_from(value).map_err(|_| format!("{key}: {value} is too many"))
                })
                .transpose()
        };

        let timeouts = table.timeouts.clone().unwrap_or_default();
        let mut timeouts_ms = BTreeMap::new();

        for (key, text) in TIMEOUTS.iter().zip([
            &timeouts.soft_timeout,
            &timeouts.hard_timeout,
            &timeouts.hard_drain_grace,
            &timeouts.shutdown_timeout,
            &timeouts.websocket_drain_grace,
            &timeouts.exit_grace,
            &timeouts.loop_stall_timeout,
            &timeouts.gil_stall_timeout,
        ]) {
            if let Some(text) = text {
                timeouts_ms.insert(
                    (*key).to_owned(),
                    parse_ms(text, &format!("timeouts.{key}"))?,
                );
            }
        }

        let memory = table.memory.clone().unwrap_or_default();
        let optional_ms = |text: &Option<String>, key: &str| {
            text.as_deref().map(|text| parse_ms(text, key)).transpose()
        };
        let optional_bytes = |text: &Option<String>, key: &str| {
            text.as_deref()
                .map(|text| parse_bytes(text, key))
                .transpose()
        };

        let settings = AppSettings {
            env,
            processes: count(table.processes, "processes")?,
            threads: count(table.threads, "threads")?,
            sync_threads: count(table.sync_threads, "sync_threads")?,
            concurrency: count(table.concurrency, "concurrency")?,
            max_requests: table.max_requests,
            max_lifetime_ms: optional_ms(&table.max_lifetime, "max_lifetime")?,
            max_surge: table.max_surge,
            max_unavailable: table.max_unavailable,
            timeouts_ms,
            memory_measure: memory.measure,
            memory_soft_limit: optional_bytes(&memory.soft_limit, "memory.soft_limit")?,
            memory_soft_for_ms: optional_ms(&memory.soft_for, "memory.soft_for")?,
            memory_reclaim_first: memory.reclaim_first,
            memory_hard_limit: optional_bytes(&memory.hard_limit, "memory.hard_limit")?,
            memory_check_interval_ms: optional_ms(&memory.check_interval, "memory.check_interval")?,
            services: table.service.clone(),
        };

        settings.check()?;
        Ok(settings)
    }

    /// Write the settings into an `[app.NAME]` table, as an export does.
    pub fn write_table(&self, table: &mut muru_config::App) {
        let timeout = |key: &str| self.timeouts_ms.get(key).copied().map(duration);

        table.env = crate::config::env_table_from(&self.env);
        table.processes = self.processes.map(|value| value as usize);
        table.threads = self.threads.map(|value| value as usize);
        table.sync_threads = self.sync_threads.map(|value| value as usize);
        table.concurrency = self.concurrency.map(|value| value as usize);
        table.max_requests = self.max_requests;
        table.max_lifetime = self.max_lifetime_ms.map(duration);
        table.max_surge = self.max_surge;
        table.max_unavailable = self.max_unavailable;
        table.timeouts = (!self.timeouts_ms.is_empty()).then(|| muru_config::HostTimeouts {
            soft_timeout: timeout("soft_timeout"),
            hard_timeout: timeout("hard_timeout"),
            hard_drain_grace: timeout("hard_drain_grace"),
            shutdown_timeout: timeout("shutdown_timeout"),
            websocket_drain_grace: timeout("websocket_drain_grace"),
            exit_grace: timeout("exit_grace"),
            loop_stall_timeout: timeout("loop_stall_timeout"),
            gil_stall_timeout: timeout("gil_stall_timeout"),
        });

        let memory = muru_config::Memory {
            measure: self.memory_measure,
            soft_limit: self.memory_soft_limit.map(muru_config::format_size),
            soft_for: self.memory_soft_for_ms.map(duration),
            reclaim_first: self.memory_reclaim_first,
            hard_limit: self.memory_hard_limit.map(muru_config::format_size),
            check_interval: self.memory_check_interval_ms.map(duration),
        };
        table.memory = (memory != muru_config::Memory::default()).then_some(memory);
        table.service = self.services.clone();
    }

    /// What can be refused before a release is deployed with them.
    fn check(&self) -> Result<(), String> {
        let zero = [
            ("processes", self.processes.map(u64::from)),
            ("threads", self.threads.map(u64::from)),
            ("sync_threads", self.sync_threads.map(u64::from)),
            ("concurrency", self.concurrency.map(u64::from)),
            ("max_requests", self.max_requests),
            ("max_lifetime", self.max_lifetime_ms),
            ("memory.check_interval", self.memory_check_interval_ms),
        ];

        // Section 7.8: with both set here, they must leave a way to swap
        // a worker; one set alone is checked with the release it joins.

        if let (Some(max_surge), Some(max_unavailable)) = (self.max_surge, self.max_unavailable) {
            muru_config::check_swap(max_surge, max_unavailable)?;
        }

        if let Some((key, _)) = zero.iter().find(|(_, value)| *value == Some(0)) {
            return Err(format!("{key} must be more than 0"));
        }

        if let (Some(soft), Some(hard)) = (self.memory_soft_limit, self.memory_hard_limit)
            && hard < soft
        {
            return Err("memory.hard_limit must not be below memory.soft_limit".to_owned());
        }

        for (name, settings) in &self.services {
            if !muru_config::valid_name(name) {
                return Err(format!("service.{name}: {name:?} is not a service's name"));
            }

            settings
                .check()
                .map_err(|error| format!("service.{name}: {error}"))?;
        }

        Ok(())
    }

    /// These settings with `over`'s laid on them: a setting `over` gives
    /// replaces this one's, a variable by its name.
    pub fn with(&self, over: &AppSettings) -> AppSettings {
        let mut env: BTreeMap<String, EnvSpec> = self
            .env
            .iter()
            .map(|entry| (entry.name.clone(), entry.clone()))
            .collect();

        for entry in &over.env {
            env.insert(entry.name.clone(), entry.clone());
        }

        let mut timeouts_ms = self.timeouts_ms.clone();
        timeouts_ms.extend(over.timeouts_ms.clone());

        AppSettings {
            env: env.into_values().collect(),
            processes: over.processes.or(self.processes),
            threads: over.threads.or(self.threads),
            sync_threads: over.sync_threads.or(self.sync_threads),
            concurrency: over.concurrency.or(self.concurrency),
            max_requests: over.max_requests.or(self.max_requests),
            max_lifetime_ms: over.max_lifetime_ms.or(self.max_lifetime_ms),
            max_surge: over.max_surge.or(self.max_surge),
            max_unavailable: over.max_unavailable.or(self.max_unavailable),
            timeouts_ms,
            memory_measure: over.memory_measure.or(self.memory_measure),
            memory_soft_limit: over.memory_soft_limit.or(self.memory_soft_limit),
            memory_soft_for_ms: over.memory_soft_for_ms.or(self.memory_soft_for_ms),
            memory_reclaim_first: over.memory_reclaim_first.or(self.memory_reclaim_first),
            memory_hard_limit: over.memory_hard_limit.or(self.memory_hard_limit),
            memory_check_interval_ms: over
                .memory_check_interval_ms
                .or(self.memory_check_interval_ms),
            services: {
                let mut services = self.services.clone();

                for (name, settings) in &over.services {
                    let merged = services
                        .get(name)
                        .map_or_else(|| settings.clone(), |under| under.with(settings));
                    services.insert(name.clone(), merged);
                }

                services
            },
        }
    }

    /// Remove a setting by its key: a scalar's (`processes`,
    /// `memory.soft_limit`), `timeouts.KEY`, `env.NAME`, or `env` for
    /// every variable. A key that names no setting is refused; one that
    /// names a setting not set is no change.
    pub fn unset(&mut self, key: &str) -> Result<(), String> {
        if key == "env" {
            self.env.clear();
            return Ok(());
        }

        if let Some(name) = key.strip_prefix("env.") {
            muru_config::check_env_name(name)?;
            self.env.retain(|entry| entry.name != name);
            return Ok(());
        }

        // Issue 1011: `service.SVC` removes all a service has set, and
        // `service.SVC.KEY` one key of it, a variable as `env.NAME`.

        if let Some(rest) = key.strip_prefix("service.") {
            let (name, inner) = rest.split_once('.').unwrap_or((rest, ""));

            let Some(settings) = self.services.get_mut(name) else {
                return Ok(());
            };

            match inner {
                "" => {
                    self.services.remove(name);
                    return Ok(());
                }
                "autostart" => settings.autostart = None,
                "instances" => settings.instances = None,
                "restart" => settings.restart = None,
                "stop_timeout" => settings.stop_timeout = None,
                "on_idle" => settings.on_idle = None,
                "env" => settings.env.clear(),
                other => match other.strip_prefix("env.") {
                    Some(variable) => {
                        settings.env.remove(variable);
                    }
                    None => {
                        return Err(format!(
                            "{key} is not an application setting: a service's are {}",
                            muru_config::SERVICE_OVERRIDE_KEYS.join(", ")
                        ));
                    }
                },
            }

            if settings.is_empty() {
                self.services.remove(name);
            }

            return Ok(());
        }

        if let Some(name) = key.strip_prefix("timeouts.") {
            if !TIMEOUTS.contains(&name) {
                return Err(format!(
                    "{key} is not an application setting: the timeouts are {}",
                    TIMEOUTS.join(", ")
                ));
            }

            self.timeouts_ms.remove(name);
            return Ok(());
        }

        match key {
            "processes" => self.processes = None,
            "threads" => self.threads = None,
            "sync_threads" => self.sync_threads = None,
            "concurrency" => self.concurrency = None,
            "max_requests" => self.max_requests = None,
            "max_lifetime" => self.max_lifetime_ms = None,
            "max_surge" => self.max_surge = None,
            "max_unavailable" => self.max_unavailable = None,
            "memory.measure" => self.memory_measure = None,
            "memory.soft_limit" => self.memory_soft_limit = None,
            "memory.soft_for" => self.memory_soft_for_ms = None,
            "memory.reclaim_first" => self.memory_reclaim_first = None,
            "memory.hard_limit" => self.memory_hard_limit = None,
            "memory.check_interval" => self.memory_check_interval_ms = None,
            other => {
                return Err(format!(
                    "{other} is not an application setting: {}, timeouts.KEY, env.NAME, env, \
                     service.NAME or service.NAME.KEY",
                    SCALARS.join(", ")
                ));
            }
        }

        Ok(())
    }

    /// Each setting as `key = value`, by the key a release records its
    /// layer under, a secret by its reference. In a stable order: the
    /// scalars, the timeouts, the memory policy, then the variables.
    pub fn lines(&self) -> Vec<(String, String)> {
        let mut lines: Vec<(String, String)> = [
            ("processes", self.processes.map(|value| value.to_string())),
            ("threads", self.threads.map(|value| value.to_string())),
            (
                "sync_threads",
                self.sync_threads.map(|value| value.to_string()),
            ),
            (
                "concurrency",
                self.concurrency.map(|value| value.to_string()),
            ),
            (
                "max_requests",
                self.max_requests.map(|value| value.to_string()),
            ),
            ("max_lifetime", self.max_lifetime_ms.map(duration)),
            ("max_surge", self.max_surge.map(|value| value.to_string())),
            (
                "max_unavailable",
                self.max_unavailable.map(|value| value.to_string()),
            ),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some((key.to_owned(), value?)))
        .collect();

        for key in TIMEOUTS {
            if let Some(ms) = self.timeouts_ms.get(key) {
                lines.push((format!("timeouts.{key}"), duration(*ms)));
            }
        }

        for (key, value) in [
            (
                "memory.measure",
                self.memory_measure
                    .map(|measure| measure.as_str().to_owned()),
            ),
            (
                "memory.soft_limit",
                self.memory_soft_limit.map(muru_config::format_size),
            ),
            ("memory.soft_for", self.memory_soft_for_ms.map(duration)),
            (
                "memory.reclaim_first",
                self.memory_reclaim_first.map(|value| value.to_string()),
            ),
            (
                "memory.hard_limit",
                self.memory_hard_limit.map(muru_config::format_size),
            ),
            (
                "memory.check_interval",
                self.memory_check_interval_ms.map(duration),
            ),
        ] {
            if let Some(value) = value {
                lines.push((key.to_owned(), value));
            }
        }

        for entry in &self.env {
            let value = match (&entry.secret_store, &entry.secret_key) {
                (Some(store), Some(key)) if entry.as_file => {
                    format!("secret {store}:{key} as a file")
                }
                (Some(store), Some(key)) => format!("secret {store}:{key}"),
                _ => entry.value.clone().unwrap_or_default(),
            };

            lines.push((crate::config::env_layer(&entry.name), value));
        }

        // Issue 1011: what is changed of each service, by its key.

        for (name, settings) in &self.services {
            let key = |inner: &str| format!("service.{name}.{inner}");

            if let Some(autostart) = settings.autostart {
                lines.push((key("autostart"), autostart.to_string()));
            }

            if let Some(instances) = settings.instances {
                lines.push((key("instances"), instances.to_string()));
            }

            if let Some(restart) = &settings.restart {
                lines.push((
                    key("restart"),
                    serde_json::to_string(restart).unwrap_or_default(),
                ));
            }

            if let Some(stop_timeout) = &settings.stop_timeout {
                lines.push((key("stop_timeout"), stop_timeout.clone()));
            }

            if let Some(on_idle) = settings.on_idle {
                lines.push((key("on_idle"), on_idle.as_str().to_owned()));
            }

            for (variable, value) in &settings.env {
                lines.push((
                    key(&format!("env.{variable}")),
                    match value {
                        muru_config::EnvValue::Plain(value) => value.clone(),
                        muru_config::EnvValue::Secret(secret) => {
                            format!("secret {}:{}", secret.secret.store, secret.secret.key)
                        }
                    },
                ));
            }
        }

        lines
    }

    /// Lay these settings under a deploy request: each the request does
    /// not give itself is given, a variable by its name. Answers with the
    /// keys given, as a release records their layers. A managed HTTP
    /// release takes only the environment, since it has no workers.
    pub fn under(&self, request: &mut pb::DeployRequest) -> Vec<String> {
        let mut given = Vec::new();

        for entry in &self.env {
            if request.env.iter().any(|own| own.name == entry.name) {
                continue;
            }

            request.env.push(pb::EnvEntry {
                name: entry.name.clone(),
                value: entry.value.clone().unwrap_or_default(),
                secret_store: entry.secret_store.clone().unwrap_or_default(),
                secret_key: entry.secret_key.clone().unwrap_or_default(),
                as_file: entry.as_file,
            });
            given.push(crate::config::env_layer(&entry.name));
        }

        if request.kind == "http" {
            return given;
        }

        // Section 7.8: the swap settings, which are counts.

        let mut fill_count = |slot: &mut Option<u32>, value: Option<u32>, key: &str| {
            if slot.is_none()
                && let Some(value) = value
            {
                *slot = Some(value);
                given.push(key.to_owned());
            }
        };

        fill_count(&mut request.max_surge, self.max_surge, "max_surge");
        fill_count(
            &mut request.max_unavailable,
            self.max_unavailable,
            "max_unavailable",
        );

        let mut fill = |slot: &mut Option<u64>, value: Option<u64>, key: &str| {
            if slot.is_none()
                && let Some(value) = value
            {
                *slot = Some(value);
                given.push(key.to_owned());
            }
        };

        let timeout = |key: &str| self.timeouts_ms.get(key).copied();

        fill(&mut request.max_requests, self.max_requests, "max_requests");
        fill(
            &mut request.max_lifetime_ms,
            self.max_lifetime_ms,
            "max_lifetime",
        );
        fill(
            &mut request.soft_timeout_ms,
            timeout("soft_timeout"),
            "timeouts.soft_timeout",
        );
        fill(
            &mut request.hard_timeout_ms,
            timeout("hard_timeout"),
            "timeouts.hard_timeout",
        );
        fill(
            &mut request.hard_drain_grace_ms,
            timeout("hard_drain_grace"),
            "timeouts.hard_drain_grace",
        );
        fill(
            &mut request.shutdown_timeout_ms,
            timeout("shutdown_timeout"),
            "timeouts.shutdown_timeout",
        );
        fill(
            &mut request.websocket_drain_grace_ms,
            timeout("websocket_drain_grace"),
            "timeouts.websocket_drain_grace",
        );
        fill(
            &mut request.exit_grace_ms,
            timeout("exit_grace"),
            "timeouts.exit_grace",
        );
        fill(
            &mut request.loop_stall_timeout_ms,
            timeout("loop_stall_timeout"),
            "timeouts.loop_stall_timeout",
        );
        fill(
            &mut request.gil_stall_timeout_ms,
            timeout("gil_stall_timeout"),
            "timeouts.gil_stall_timeout",
        );
        fill(
            &mut request.memory_soft_limit,
            self.memory_soft_limit,
            "memory.soft_limit",
        );
        fill(
            &mut request.memory_soft_for_ms,
            self.memory_soft_for_ms,
            "memory.soft_for",
        );
        fill(
            &mut request.memory_hard_limit,
            self.memory_hard_limit,
            "memory.hard_limit",
        );
        fill(
            &mut request.memory_check_interval_ms,
            self.memory_check_interval_ms,
            "memory.check_interval",
        );

        for (slot, value, key) in [
            (&mut request.processes, self.processes, "processes"),
            (&mut request.threads, self.threads, "threads"),
            (&mut request.sync_threads, self.sync_threads, "sync_threads"),
            (&mut request.concurrency, self.concurrency, "concurrency"),
        ] {
            if slot.is_none()
                && let Some(value) = value
            {
                *slot = Some(value);
                given.push(key.to_owned());
            }
        }

        if request.memory_reclaim_first.is_none()
            && let Some(value) = self.memory_reclaim_first
        {
            request.memory_reclaim_first = Some(value);
            given.push("memory.reclaim_first".to_owned());
        }

        if request.memory_measure.is_empty()
            && let Some(measure) = self.memory_measure
        {
            request.memory_measure = measure.as_str().to_owned();
            given.push("memory.measure".to_owned());
        }

        given
    }
}

/// Resolve a deploy request with the application's settings under it, and
/// record the keys they gave as the layer `the application`.
pub fn resolve(
    request: &pb::DeployRequest,
    settings: &AppSettings,
    tables: &crate::config::Tables<'_>,
    provisioning: &crate::provision::Provisioning,
) -> Result<crate::model::ReleaseSpec, String> {
    let mut request = request.clone();
    let given = settings.under(&mut request);
    let mut spec = crate::config::resolve_spec(&request, tables, provisioning)?;

    for key in given {
        spec.layers.0.insert(key, LAYER.to_owned());
    }

    // Issue 1011: what the node changes of the release's services is laid
    // over the tables the release declares; a service the release does
    // not declare has nothing to change. The result is checked as the
    // project's own table was.

    for (name, settings) in &settings.services {
        let Some(table) = spec.services.get_mut(name) else {
            continue;
        };

        for key in settings.apply(table) {
            spec.layers
                .0
                .insert(format!("services.{name}.{key}"), LAYER.to_owned());
        }

        muru_config::check_project_service(table)
            .map_err(|error| format!("service.{name}: {error}"))?;
    }

    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::{AppSettings, pb};

    #[test]
    fn a_services_settings_are_set_shown_merged_and_removed_by_key() {
        let message = |services: &[(&str, &str)]| pb::AppSettings {
            services: services
                .iter()
                .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
                .collect(),
            ..pb::AppSettings::default()
        };
        let first = AppSettings::from_message(&message(&[
            ("worker", r#"{"instances": 8, "env": {"QUEUE": "bulk"}}"#),
            ("beat", r#"{"autostart": false}"#),
        ]))
        .unwrap();
        assert_eq!(
            first.lines(),
            [
                ("service.beat.autostart".to_owned(), "false".to_owned()),
                ("service.worker.instances".to_owned(), "8".to_owned()),
                ("service.worker.env.QUEUE".to_owned(), "bulk".to_owned()),
            ]
        );

        // They go to a message and back, and to a table and back.

        assert_eq!(
            AppSettings::from_message(&first.to_message()).unwrap(),
            first
        );
        let mut table = muru_config::App::default();
        first.write_table(&mut table);
        assert_eq!(table.service["worker"].instances, Some(8));
        assert_eq!(AppSettings::from_table(&table).unwrap(), first);

        // A later setting replaces one key of a service and keeps the rest.

        let over = AppSettings::from_message(&message(&[("worker", r#"{"stop_timeout": "2m"}"#)]))
            .unwrap();
        let mut merged = first.with(&over);
        assert_eq!(merged.services["worker"].instances, Some(8));
        assert_eq!(
            merged.services["worker"].stop_timeout.as_deref(),
            Some("2m")
        );

        // One key is removed, then a variable, then a whole service.

        merged.unset("service.worker.instances").unwrap();
        merged.unset("service.worker.env.QUEUE").unwrap();
        assert_eq!(
            merged.lines(),
            [
                ("service.beat.autostart".to_owned(), "false".to_owned()),
                ("service.worker.stop_timeout".to_owned(), "2m".to_owned()),
            ]
        );
        merged.unset("service.beat").unwrap();
        merged.unset("service.worker.stop_timeout").unwrap();
        assert!(merged.is_empty());
        assert!(merged.clone().unset("service.worker.command").is_ok());
        let mut again = first.clone();
        assert!(
            again
                .unset("service.worker.command")
                .unwrap_err()
                .starts_with("service.worker.command is not an application setting")
        );

        // What cannot be taken is refused when it is set.

        assert!(
            AppSettings::from_message(&message(&[("worker", r#"{"instances": 0}"#)]))
                .unwrap_err()
                .starts_with("service.worker: instances must be at least 1")
        );
        assert!(
            AppSettings::from_message(&message(&[("worker", r#"{"command": ["x"]}"#)]))
                .unwrap_err()
                .starts_with("service.worker: ")
        );
    }

    fn settings() -> AppSettings {
        AppSettings::from_message(&pb::AppSettings {
            env: vec![
                pb::EnvEntry {
                    name: "DATABASE_URL".to_owned(),
                    value: "postgres://db/shop".to_owned(),
                    ..Default::default()
                },
                pb::EnvEntry {
                    name: "API_KEY".to_owned(),
                    secret_store: "vault".to_owned(),
                    secret_key: "shop/api".to_owned(),
                    ..Default::default()
                },
            ],
            processes: Some(4),
            hard_timeout_ms: Some(60_000),
            memory_soft_limit: Some(800 * 1024 * 1024),
            ..Default::default()
        })
        .expect("valid settings")
    }

    #[test]
    fn a_request_keeps_what_it_gives_and_takes_the_rest() {
        let mut request = pb::DeployRequest {
            processes: Some(2),
            env: vec![pb::EnvEntry {
                name: "DATABASE_URL".to_owned(),
                value: "postgres://db/trial".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let given = settings().under(&mut request);

        assert_eq!(request.processes, Some(2));
        assert_eq!(request.hard_timeout_ms, Some(60_000));
        assert_eq!(request.env.len(), 2);
        assert_eq!(request.env[0].value, "postgres://db/trial");
        assert_eq!(request.env[1].secret_key, "shop/api");
        assert_eq!(
            given,
            ["env.API_KEY", "timeouts.hard_timeout", "memory.soft_limit"]
        );

        // A managed server takes the environment alone.

        let mut http = pb::DeployRequest {
            kind: "http".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            settings().under(&mut http),
            ["env.API_KEY", "env.DATABASE_URL"]
        );
        assert_eq!(http.processes, None);
    }

    #[test]
    fn settings_round_trip_through_the_table_and_the_message() {
        let settings = settings();
        let mut table = muru_config::App::default();
        settings.write_table(&mut table);

        assert_eq!(table.processes, Some(4));
        assert_eq!(
            table
                .timeouts
                .as_ref()
                .and_then(|t| t.hard_timeout.as_deref()),
            Some("60s")
        );
        assert_eq!(AppSettings::from_table(&table).unwrap(), settings);
        assert_eq!(
            AppSettings::from_message(&settings.to_message()).unwrap(),
            settings
        );
    }

    #[test]
    fn unset_removes_by_key_and_refuses_what_is_not_one() {
        let mut settings = settings();

        settings.unset("env.API_KEY").unwrap();
        settings.unset("timeouts.hard_timeout").unwrap();
        settings.unset("processes").unwrap();
        assert_eq!(
            settings.lines(),
            [
                ("memory.soft_limit".to_owned(), "800MB".to_owned()),
                (
                    "env.DATABASE_URL".to_owned(),
                    "postgres://db/shop".to_owned()
                ),
            ]
        );

        assert!(
            settings
                .unset("entrypoint")
                .unwrap_err()
                .contains("not an application setting")
        );
        assert!(settings.unset("timeouts.queue_timeout").is_err());

        settings.unset("env").unwrap();
        settings.unset("memory.soft_limit").unwrap();
        assert!(settings.is_empty());
    }

    #[test]
    fn a_count_of_nothing_is_refused() {
        let error = AppSettings::from_message(&pb::AppSettings {
            threads: Some(0),
            ..Default::default()
        })
        .unwrap_err();

        assert_eq!(error, "threads must be more than 0");
    }
}
