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
