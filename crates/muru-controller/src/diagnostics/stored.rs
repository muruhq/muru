//! What a daemon's state store holds, read without the daemon: its
//! releases, their effective settings, its events and the names it knows.
//! Every function only reads, so a store opened read-only serves.

use std::collections::BTreeSet;

use muru_proto::admin::v1 as pb;

use super::{ReleaseFacts, Setting, event_message};
use crate::config;
use crate::model::Release;
use crate::store::{Store, StoreError};

/// How many events the store is asked for at once: more than it keeps.
const EVERY_EVENT: usize = 100_000;

/// Every release the store records, newest first, as the diagnostics
/// describe them. The store keeps no workers and no detected interface,
/// which only a running host has.
pub fn releases(store: &Store) -> Result<Vec<ReleaseFacts>, StoreError> {
    let serving: BTreeSet<(String, String)> = store
        .apps()?
        .into_iter()
        .filter_map(|app| Some((app.name, app.current?)))
        .collect();

    Ok(store
        .releases(None)?
        .iter()
        .map(|release| {
            let current = serving.contains(&(release.app.clone(), release.id.clone()));
            release_facts(release, current)
        })
        .collect())
}

/// A stored release as the diagnostics describe it.
fn release_facts(release: &Release, serving: bool) -> ReleaseFacts {
    let interpreter = release.spec.interpreter.as_ref();

    ReleaseFacts {
        app: release.app.clone(),
        id: release.id.clone(),
        state: release.state.as_str().to_owned(),
        message: release.message.clone(),
        serving,
        kind: if release.spec.http.is_some() {
            "http".to_owned()
        } else {
            "workers".to_owned()
        },
        entrypoint: release.spec.entrypoint.clone(),
        instrumentation: release
            .spec
            .instrumentation
            .iter()
            .map(|entry| entry.module.clone())
            .collect(),
        interface: release.spec.interface.as_str().to_owned(),
        detected_interface: String::new(),
        frameworks: Vec::new(),
        python_version: interpreter
            .map(|interpreter| interpreter.version.clone())
            .unwrap_or_default(),
        python_build: interpreter
            .map(|interpreter| {
                if interpreter.free_threaded {
                    "free-threaded"
                } else {
                    "regular"
                }
            })
            .unwrap_or_default()
            .to_owned(),
        workers: Vec::new(),
    }
}

/// The effective settings of one release with the layer each came from
/// (section 3.3), and the limits it, its worker groups and its mounts set
/// on the requests they serve (section 4.6): what `muructl config show
/// --effective` lists for it. A managed HTTP server has none of the
/// worker settings, and a release made before layers were recorded has
/// no source to give.
pub fn settings(store: &Store, app: &str, id: &str) -> Result<Vec<Setting>, StoreError> {
    let release = store.release(app, id)?;
    let spec = &release.spec;
    let scope = format!("{app}@{id}");
    let mut settings = Vec::new();

    if spec.http.is_none() {
        for (key, value) in config::release_settings(spec) {
            let source = spec
                .layers
                .0
                .get(key)
                .map_or("not recorded", String::as_str);

            settings.push(Setting {
                scope: scope.clone(),
                key: key.to_owned(),
                value,
                source: source.to_owned(),
            });
        }
    }

    let served = |scope: String, source: &str, limits: &crate::model::ServedLimits| {
        config::served_settings(limits)
            .into_iter()
            .map(move |(key, value)| Setting {
                scope: scope.clone(),
                key,
                value,
                source: source.to_owned(),
            })
            .collect::<Vec<_>>()
    };

    settings.extend(served(scope.clone(), "the release", &spec.limits));

    for group in &spec.groups {
        settings.extend(served(
            format!("{scope} group {}", group.name),
            "the worker group",
            &group.limits,
        ));
    }

    for mount in &spec.mounts {
        settings.extend(served(
            format!("{scope} mount {}", mount.name),
            "the mount",
            &mount.limits,
        ));
    }

    Ok(settings)
}

/// Everything the store records of one release, for an archive: what it
/// is, its effective settings with where each came from, and its whole
/// specification, in which the value of a variable whose name says it
/// is a secret is masked, as is the password of a URL. A secret from a
/// store is recorded only as its store and key.
pub fn release_record(
    store: &Store,
    release: &ReleaseFacts,
) -> Result<serde_json::Value, StoreError> {
    let stored = store.release(&release.app, &release.id)?;
    let mut spec = serde_json::to_value(&stored.spec)
        .map_err(|error| StoreError::Decode(error.to_string()))?;

    for variable in spec["env"].as_array_mut().into_iter().flatten() {
        let name = variable["name"].as_str().unwrap_or_default().to_owned();

        if let Some(value) = variable.get_mut("value")
            && let Some(written) = value.as_str()
        {
            *value = super::mask::value(&name, written).into();
        }
    }

    Ok(serde_json::json!({
        "release": release,
        "created_unix_ms": stored.created_unix_ms,
        "settings": settings(store, &release.app, &release.id)?,
        "spec": spec,
    }))
}

/// What is in force outside the configuration: each rollout and where it
/// stands, the sizes releases are pinned at, the applications and
/// virtual hosts in maintenance mode, and each overlay's state.
pub fn in_force(store: &Store) -> Result<serde_json::Value, StoreError> {
    let mut pins = Vec::new();

    for release in store.releases(None)? {
        for (group, processes) in store.scale_pins(&release.app, &release.id)? {
            pins.push(serde_json::json!({
                "release": format!("{}@{}", release.app, release.id),
                "group": group,
                "processes": processes,
            }));
        }
    }

    let since = |held: Vec<(String, u64)>| -> Vec<serde_json::Value> {
        held.into_iter()
            .map(|(name, since)| serde_json::json!({"name": name, "since_unix_ms": since}))
            .collect()
    };

    let overlays: Vec<serde_json::Value> = store
        .overlay_states()?
        .into_iter()
        .map(|state| {
            serde_json::json!({
                "name": state.name,
                "active": state.active,
                "source": state.source,
                "since_unix_ms": state.since_unix_ms,
                "until_unix_ms": state.until_unix_ms,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "rollouts": store.rollouts(None)?,
        "scale_pins": pins,
        "maintenance": {
            "applications": since(store.maintenance()?),
            "hosts": since(store.host_maintenance()?),
        },
        "overlays": overlays,
    }))
}

/// The effective configuration as `muructl config show --effective`
/// writes it: desired state as the active overlays leave it, as TOML,
/// headed by what each overlay sets that is in force. For `app` alone
/// when one is given. The variables a release's table writes with their
/// values are masked as a worker's environment is.
pub fn effective(store: &Store, app: Option<&str>) -> Result<String, String> {
    let mut document = config::export(store, app).map_err(|error| error.to_string())?;
    let attributions =
        config::make_effective(store, &mut document, app).map_err(|error| error.to_string())?;
    let mut text = String::new();

    if attributions.is_empty() {
        text.push_str("# No active overlay changes this configuration.\n\n");
    } else {
        text.push_str("# The active overlays set, and the routes below include:\n");

        for line in &attributions {
            text.push_str(&format!("#   {line}\n"));
        }

        text.push('\n');
    }

    let document = document.to_toml().map_err(|error| error.to_string())?;
    text.push_str(&super::mask::document_env(&document)?);
    Ok(text)
}

/// Every event the store keeps, oldest first.
pub fn events(store: &Store) -> Result<Vec<pb::Event>, StoreError> {
    Ok(store
        .events_after(0, EVERY_EVENT)?
        .into_iter()
        .map(event_message)
        .collect())
}

/// The names a store knows that identify an installation: its
/// applications, its virtual hosts, and the users and groups its
/// applications run as.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    /// The applications.
    pub apps: Vec<String>,
    /// The virtual hosts, by the names requests are matched against.
    pub hosts: Vec<String>,
    /// The users and groups applications run as.
    pub users: Vec<String>,
}

/// The names the store knows.
pub fn names(store: &Store) -> Result<Names, StoreError> {
    let apps = store.apps()?;
    let mut hosts = BTreeSet::new();

    for (name, host) in store.hosts()? {
        hosts.insert(name);
        hosts.extend(host.names);
        hosts.extend(host.canonical);
    }

    Ok(Names {
        users: apps
            .iter()
            .flat_map(|app| [app.user.clone(), app.group.clone()])
            .flatten()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        apps: apps.into_iter().map(|app| app.name).collect(),
        hosts: hosts.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReleaseState;

    fn store() -> Store {
        let store = Store::in_memory().unwrap();
        store.ensure_app("shop").unwrap();

        let mut spec = crate::store::tests::spec();
        spec.layers
            .0
            .insert("processes".to_owned(), "the deploy command".to_owned());
        spec.limits.max_body_size = Some(1 << 20);
        store.create_release("shop", "r1", &spec).unwrap();
        store.create_release("shop", "r2", &spec).unwrap();
        store
            .set_release_state("shop", "r2", ReleaseState::Ready, "")
            .unwrap();
        store
            .set_app_releases("shop", Some("r2"), Some("r1"))
            .unwrap();
        store
    }

    #[test]
    fn the_stored_releases_say_which_serves() {
        let releases = releases(&store()).unwrap();
        let named: Vec<(String, &str, bool)> = releases
            .iter()
            .map(|release| (release.name(), release.state.as_str(), release.serving))
            .collect();

        assert_eq!(named.len(), 2);
        assert!(named.contains(&("shop@r2".to_owned(), "ready", true)));
        assert!(named.contains(&("shop@r1".to_owned(), "pending", false)));
        assert!(releases.iter().all(|release| release.workers.is_empty()));
        assert!(releases.iter().all(|release| release.interface == "auto"));
    }

    #[test]
    fn a_releases_settings_carry_the_layer_each_came_from() {
        let settings = settings(&store(), "shop", "r2").unwrap();
        let of = |key: &str| settings.iter().find(|setting| setting.key == key).unwrap();

        assert_eq!(of("processes").value, "2");
        assert_eq!(of("processes").source, "the deploy command");
        assert_eq!(of("threads").source, "not recorded");
        assert_eq!(of("max_body_size").value, "1MB");
        assert_eq!(of("max_body_size").source, "the release");
        assert!(settings.iter().all(|setting| setting.scope == "shop@r2"));
    }

    #[test]
    fn a_releases_record_masks_what_looks_like_a_secret() {
        let store = store();
        let mut spec = crate::store::tests::spec();
        let variable = |name: &str, value: &str| crate::model::EnvSpec {
            name: name.to_owned(),
            value: Some(value.to_owned()),
            secret_store: None,
            secret_key: None,
            as_file: false,
        };
        spec.env = vec![
            variable("API_TOKEN", "abc123"),
            variable("DATABASE_URL", "postgres://shop:hunter2@db/shop"),
            variable("MODE", "production"),
            crate::model::EnvSpec {
                name: "STRIPE".to_owned(),
                value: None,
                secret_store: Some("file".to_owned()),
                secret_key: Some("stripe".to_owned()),
                as_file: false,
            },
        ];
        store.create_release("shop", "r3", &spec).unwrap();

        let release = releases(&store)
            .unwrap()
            .into_iter()
            .find(|release| release.id == "r3")
            .unwrap();
        let record = release_record(&store, &release).unwrap();

        assert_eq!(record["release"]["app"], "shop");
        assert_eq!(record["spec"]["entrypoint"], "shop.wsgi:application");
        assert!(record["settings"].as_array().unwrap().len() > 20);
        assert_eq!(
            record["spec"]["env"],
            serde_json::json!([
                {"name": "API_TOKEN", "value": "***"},
                {"name": "DATABASE_URL", "value": "postgres://shop:***@db/shop"},
                {"name": "MODE", "value": "production"},
                {"name": "STRIPE", "secret_store": "file", "secret_key": "stripe"},
            ])
        );
        assert!(!record.to_string().contains("hunter2"));
    }

    #[test]
    fn what_is_in_force_outside_configuration_is_read() {
        let store = store();
        store.put_scale_pin("shop", "r2", "default", 6).unwrap();
        store.set_maintenance("shop", Some(1234)).unwrap();
        store
            .set_overlay_state(
                "sale",
                Some(&crate::model::OverlayState {
                    name: "sale".to_owned(),
                    active: true,
                    source: "manual".to_owned(),
                    since_unix_ms: 1000,
                    until_unix_ms: Some(2000),
                }),
            )
            .unwrap();

        let found = in_force(&store).unwrap();
        assert_eq!(
            found["scale_pins"],
            serde_json::json!([{"release": "shop@r2", "group": "default", "processes": 6}])
        );
        assert_eq!(
            found["maintenance"],
            serde_json::json!({
                "applications": [{"name": "shop", "since_unix_ms": 1234}],
                "hosts": [],
            })
        );
        assert_eq!(found["overlays"][0]["name"], "sale");
        assert_eq!(found["overlays"][0]["until_unix_ms"], 2000);
        assert_eq!(found["rollouts"], serde_json::json!([]));

        // The effective configuration is the document the command shows.

        let text = effective(&store, None).unwrap();
        assert!(text.starts_with("# No active overlay changes this configuration.\n\n"));
        assert!(text.contains("[app.shop"));
        assert!(effective(&store, Some("nope")).is_err());
    }

    #[test]
    fn the_stores_events_and_names_are_read() {
        let store = store();
        store
            .append_event(
                "release.ready",
                "shop@r2",
                "info",
                "release shop@r2 is ready",
            )
            .unwrap();

        let events = events(&store).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].r#type, "release.ready");
        assert_eq!(events[0].subject, "shop@r2");

        let host = muru_config::Host {
            names: vec!["shop.example.com".to_owned()],
            ..muru_config::Host::default()
        };
        store.put_host("shop", &host).unwrap();
        store
            .set_app_identity("shop", Some("shopuser"), Some("shopgroup"), None)
            .unwrap();

        let names = names(&store).unwrap();
        assert_eq!(names.apps, ["shop"]);
        assert_eq!(names.hosts, ["shop", "shop.example.com"]);
        assert_eq!(names.users, ["shopgroup", "shopuser"]);
    }
}
