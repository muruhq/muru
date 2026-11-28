//! What the objects of a deployment name (design specification section
//! 10.13, ADR 0080): one function for each kind of object lists what it
//! refers to, and an apply is refused when something that stays names
//! something the apply removes, whichever kind names it and whichever
//! deployment holds it.
//!
//! A reference added to the format is added to its kind's function here,
//! and the check follows. A condition's labels are not references: a
//! label filters figures and resolves nothing (ADR 0079).

use std::collections::{BTreeMap, BTreeSet};

use muru_config::{Host, Overlay, RateLimit, Route, RouteOverlay};

use crate::config::{Action, Plan};
use crate::deployments::{Declared, Kind};
use crate::store::Store;

/// Something an object names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    /// An object by its name: a host, an application, an upstream, a
    /// client class, a rate limit or a condition.
    Object(Kind, String),
    /// A release, as its application and id.
    Release(String, String),
    /// A route by its `name`, as an overlay names it.
    NamedRoute(String),
}

impl Target {
    /// The target as a message says it.
    fn describe(&self) -> String {
        match self {
            Target::Object(kind, name) => format!("{} {name}", kind.as_str()),
            Target::Release(app, id) => format!("release {app}@{id}"),
            Target::NamedRoute(name) => format!("the route named {name}"),
        }
    }
}

fn object(kind: Kind, name: &str) -> Target {
    Target::Object(kind, name.to_owned())
}

/// What a traffic policy's entry names: a release of `app`, or with the
/// `upstream:` prefix an upstream. A release of no application is nothing.
fn traffic_target(app: Option<&str>, id: &str) -> Option<Target> {
    match id.strip_prefix(muru_edge::routing::UPSTREAM_PREFIX) {
        Some(upstream) => Some(object(Kind::Upstream, upstream)),
        None => app.map(|app| Target::Release(app.to_owned(), id.to_owned())),
    }
}

/// What a route names: its host, what serves it, falls back for it and
/// decides for it, the client classes it takes, its rate limits, the
/// applications whose redirects it accepts, and the releases, upstreams
/// and classes of its traffic.
pub fn route(route: &Route) -> Vec<Target> {
    let mut named = vec![object(Kind::Host, &route.host)];
    let fallback_app = route
        .fallback
        .as_ref()
        .and_then(|fallback| fallback.app.as_deref());
    let fallback_upstream = route
        .fallback
        .as_ref()
        .and_then(|fallback| fallback.upstream.as_deref());
    let auth = route.auth.as_ref();
    let apps = [
        Some(route.app.as_str()),
        fallback_app,
        auth.map(|auth| auth.app.as_str()),
    ];
    let upstreams = [
        Some(route.upstream.as_str()),
        fallback_upstream,
        auth.map(|auth| auth.upstream.as_str()),
    ];

    named.extend(
        apps.into_iter()
            .flatten()
            .chain(route.accept_redirects_from.iter().map(String::as_str))
            .filter(|app| !app.is_empty())
            .map(|app| object(Kind::App, app)),
    );
    named.extend(
        upstreams
            .into_iter()
            .flatten()
            .filter(|upstream| !upstream.is_empty())
            .map(|upstream| object(Kind::Upstream, upstream)),
    );
    named.extend(
        route
            .classes()
            .iter()
            .map(|class| object(Kind::ClientClass, class)),
    );
    named.extend(
        route
            .rate_limits
            .iter()
            .map(|limit| object(Kind::RateLimit, limit)),
    );

    if let Some(traffic) = &route.traffic {
        // The releases are the route's application's, or a static
        // route's fallback application's.

        let app = Some(route.app.as_str())
            .filter(|app| !app.is_empty())
            .or(fallback_app);

        named.extend(
            traffic
                .split
                .keys()
                .chain(traffic.classes.values())
                .filter_map(|id| traffic_target(app, id)),
        );
        named.extend(
            traffic
                .classes
                .keys()
                .map(|class| object(Kind::ClientClass, class)),
        );
    }

    named
}

/// What a host names: its rate limits.
pub fn host(host: &Host) -> Vec<Target> {
    host.rate_limits
        .iter()
        .map(|limit| object(Kind::RateLimit, limit))
        .collect()
}

/// What a rate limit names: the client classes it is for.
pub fn rate_limit(limit: &RateLimit) -> Vec<Target> {
    limit
        .matches
        .iter()
        .flat_map(|matches| &matches.client_class)
        .map(|class| object(Kind::ClientClass, class))
        .collect()
}

/// What a release's worker group names: the client classes and the hosts
/// it takes. A host there may also be a name a host answers to, which
/// names no object and is never among what an apply removes.
pub fn group(classes: &[String], hosts: &[String]) -> Vec<Target> {
    classes
        .iter()
        .map(|class| object(Kind::ClientClass, class))
        .chain(hosts.iter().map(|host| object(Kind::Host, host)))
        .collect()
}

/// What an overlay's changes to one route name, its client classes'
/// routes included. `app` is the route's application.
fn route_overlay(changes: &RouteOverlay, app: Option<&str>, named: &mut Vec<Target>) {
    if let Some(target) = &changes.target {
        named.extend(target.app.iter().map(|app| object(Kind::App, app)));
        named.extend(
            target
                .upstream
                .iter()
                .map(|upstream| object(Kind::Upstream, upstream)),
        );
    }

    named.extend(
        changes
            .rate_limits
            .iter()
            .flatten()
            .map(|limit| object(Kind::RateLimit, limit)),
    );

    if let Some(traffic) = &changes.traffic {
        let split = traffic.split.iter().flat_map(|split| split.keys());
        named.extend(
            traffic
                .pin
                .iter()
                .chain(split)
                .filter_map(|id| traffic_target(app, id)),
        );
    }

    for (class, changes) in changes.client_routes.iter().flatten() {
        named.push(object(Kind::ClientClass, class));
        route_overlay(changes, app, named);
    }
}

/// What an overlay names: its condition, the applications and the routes
/// it changes, what its changes to each route name, and the releases of
/// its precondition and its warm-up. `route_apps` maps a route's name to
/// its application.
pub fn overlay(overlay: &Overlay, route_apps: &BTreeMap<String, String>) -> Vec<Target> {
    let mut named = Vec::new();

    named.extend(
        overlay
            .trigger
            .iter()
            .filter_map(|trigger| trigger.condition.as_deref())
            .map(|condition| object(Kind::Condition, condition)),
    );
    named.extend(overlay.apps.keys().map(|app| object(Kind::App, app)));

    for (name, changes) in &overlay.routes {
        named.push(Target::NamedRoute(name.clone()));
        let app = route_apps
            .get(name)
            .map(String::as_str)
            .filter(|app| !app.is_empty());
        route_overlay(changes, app, &mut named);
    }

    let ready = overlay
        .preconditions
        .as_ref()
        .and_then(|preconditions| preconditions.release_ready.as_deref());
    let warm = overlay
        .warm_up
        .as_ref()
        .and_then(|warm_up| warm_up.start_release.as_deref());

    named.extend(
        ready
            .into_iter()
            .chain(warm)
            .filter_map(|release| crate::config::precondition_release(release, overlay, route_apps))
            .map(|(app, id)| Target::Release(app, id)),
    );

    named
}

/// Something that stays after an apply, with what it names.
struct Staying {
    /// How a message says it, such as `route shop /api`.
    what: String,
    /// The deployment that holds it, or none when the files of this apply
    /// declare it.
    held_by: Option<String>,
    names: Vec<Target>,
}

/// Why a plan cannot go ahead for what it would leave behind: one reason
/// for each object that stays, in the files or in the store, and names
/// something the plan removes.
pub fn left_naming_removed(store: &Store, declared: &Declared, plan: &Plan) -> Vec<String> {
    let document = &declared.document;
    let mut removed: BTreeSet<Target> = BTreeSet::new();
    let mut removed_routes: BTreeSet<(String, String)> = BTreeSet::new();
    let mut removed_overlays: BTreeSet<&str> = BTreeSet::new();

    for change in &plan.changes {
        match &change.action {
            Action::RemoveHost { name } => removed.insert(object(Kind::Host, name)),
            Action::RemoveApp { app } => removed.insert(object(Kind::App, app)),
            Action::RemoveUpstream { name } => removed.insert(object(Kind::Upstream, name)),
            Action::RemoveService { name } => removed.insert(object(Kind::Service, name)),
            Action::RemoveClientClass { name } => removed.insert(object(Kind::ClientClass, name)),
            Action::RemoveRateLimit { name } => removed.insert(object(Kind::RateLimit, name)),
            Action::RemoveCondition { name } => removed.insert(object(Kind::Condition, name)),
            Action::RemoveRelease { app, id } => {
                removed.insert(Target::Release(app.clone(), id.clone()))
            }
            Action::RemoveRoute { host, path } => {
                removed_routes.insert((host.clone(), path.clone()))
            }
            Action::RemoveOverlay { name } => removed_overlays.insert(name),
            _ => false,
        };
    }

    if removed.is_empty() && removed_routes.is_empty() {
        return Vec::new();
    }

    let owners = store.ownership().unwrap_or_default();
    let holder = |kind: Kind, key: &str| {
        Some(
            owners
                .get(&(kind, key.to_owned()))
                .map_or(crate::deployments::DEFAULT, String::as_str)
                .to_owned(),
        )
    };

    // The routes as they will be: what the files declare, and what the
    // store holds that the files do not and the plan leaves.

    let stored_routes: Vec<Route> = store
        .routes()
        .unwrap_or_default()
        .into_iter()
        .filter(|route| {
            !document
                .route
                .iter()
                .any(|declared| declared.host == route.host && declared.key() == route.key())
        })
        .collect();
    let (going, kept_routes): (Vec<&Route>, Vec<&Route>) = stored_routes
        .iter()
        .partition(|route| removed_routes.contains(&(route.host.clone(), route.key())));
    let route_apps: BTreeMap<String, String> = document
        .route
        .iter()
        .chain(kept_routes.iter().copied())
        .filter_map(|route| route.name.clone().map(|name| (name, route.app.clone())))
        .collect();

    // A route's name goes with it unless a route that stays has the name.

    removed.extend(
        going
            .iter()
            .filter_map(|route| route.name.clone())
            .filter(|name| !route_apps.contains_key(name))
            .map(Target::NamedRoute),
    );

    let mut staying: Vec<Staying> = Vec::new();

    for (route, stored) in document
        .route
        .iter()
        .map(|route| (route, false))
        .chain(kept_routes.iter().map(|route| (*route, true)))
    {
        staying.push(Staying {
            what: format!("route {} {}", route.host, route.key()),
            held_by: stored
                .then(|| {
                    holder(
                        Kind::Route,
                        &crate::deployments::route_key(&route.host, &route.key()),
                    )
                })
                .flatten(),
            names: self::route(route),
        });
    }

    let stored_hosts = store.hosts().unwrap_or_default();

    for (name, table, stored) in document
        .host
        .iter()
        .map(|(name, table)| (name, table, false))
        .chain(
            stored_hosts
                .iter()
                .filter(|(name, _)| !document.host.contains_key(name))
                .map(|(name, table)| (name, table, true)),
        )
    {
        if removed.contains(&object(Kind::Host, name)) {
            continue;
        }

        staying.push(Staying {
            what: format!("host {name}"),
            held_by: stored.then(|| holder(Kind::Host, name)).flatten(),
            names: host(table),
        });
    }

    let stored_limits = store.rate_limits().unwrap_or_default();

    for (name, table, stored) in document
        .rate_limit
        .iter()
        .map(|(name, table)| (name, table, false))
        .chain(
            stored_limits
                .iter()
                .filter(|(name, _)| !document.rate_limit.contains_key(name))
                .map(|(name, table)| (name, table, true)),
        )
    {
        if removed.contains(&object(Kind::RateLimit, name)) {
            continue;
        }

        staying.push(Staying {
            what: format!("rate_limit {name}"),
            held_by: stored.then(|| holder(Kind::RateLimit, name)).flatten(),
            names: rate_limit(table),
        });
    }

    let stored_overlays = store.overlays().unwrap_or_default();

    for (name, table, stored) in document
        .overlay
        .iter()
        .map(|(name, table)| (name, table, false))
        .chain(
            stored_overlays
                .iter()
                .filter(|(name, _)| !document.overlay.contains_key(name))
                .map(|(name, table)| (name, table, true)),
        )
    {
        if removed_overlays.contains(name.as_str()) {
            continue;
        }

        staying.push(Staying {
            what: format!("overlay {name}"),
            held_by: stored.then(|| holder(Kind::Overlay, name)).flatten(),
            names: overlay(table, &route_apps),
        });
    }

    // Worker groups: those of the releases the files declare, and of the
    // stored releases the files do not declare and the plan leaves.

    for (app, table) in &document.app {
        for (id, release) in &table.release {
            for (name, group) in &release.groups {
                let Some(matches) = &group.matches else {
                    continue;
                };

                staying.push(Staying {
                    what: format!("{app}@{id}: worker group {name}"),
                    held_by: None,
                    names: self::group(&matches.client_class, &matches.host),
                });
            }
        }
    }

    for release in store.releases(None).unwrap_or_default() {
        let in_files = document
            .app
            .get(&release.app)
            .is_some_and(|table| table.release.contains_key(&release.id));

        if in_files || removed.contains(&Target::Release(release.app.clone(), release.id.clone())) {
            continue;
        }

        for group in &release.spec.groups {
            staying.push(Staying {
                what: format!(
                    "{}@{}: worker group {}",
                    release.app, release.id, group.name
                ),
                held_by: holder(Kind::App, &release.app),
                names: self::group(&group.client_class, &group.host),
            });
        }
    }

    let mut reasons = Vec::new();

    for object in &staying {
        let named: BTreeSet<&Target> = object
            .names
            .iter()
            .filter(|target| removed.contains(target))
            .collect();

        for target in named {
            reasons.push(match &object.held_by {
                Some(deployment) => format!(
                    "{} is not removed: {} of deployment {deployment} names it; take that out \
                     first, or apply both deployments together",
                    target.describe(),
                    object.what
                ),
                None => format!(
                    "{}: names {}, which the files do not declare, so --prune would remove it; \
                     declare it, or take the name out",
                    object.what,
                    target.describe()
                ),
            });
        }
    }

    reasons
}

/// What the objects a store holds name that the store does not hold:
/// one line for each, such as a route whose application was removed. A
/// store that an apply or a command left whole has none, so each is a
/// fault of Muru's own, which `muructl doctor store` reports.
pub fn dangling(store: &Store) -> Result<Vec<String>, crate::store::StoreError> {
    let mut held: BTreeSet<Target> = BTreeSet::new();
    let apps = store.apps()?;
    let releases = store.releases(None)?;
    let routes = store.routes()?;

    held.extend(apps.iter().map(|app| object(Kind::App, &app.name)));
    held.extend(
        releases
            .iter()
            .map(|release| Target::Release(release.app.clone(), release.id.clone())),
    );
    held.extend(
        routes
            .iter()
            .filter_map(|route| route.name.clone())
            .map(Target::NamedRoute),
    );

    for (name, _) in store.hosts()? {
        held.insert(object(Kind::Host, &name));
    }

    for (name, _) in store.upstreams()? {
        held.insert(object(Kind::Upstream, &name));
    }

    for stored in store.services()? {
        held.insert(object(Kind::Service, &stored.name));
    }

    for (_, class) in store.client_classes()? {
        held.insert(object(Kind::ClientClass, &class.name));
    }

    for (name, _) in store.rate_limits()? {
        held.insert(object(Kind::RateLimit, &name));
    }

    for (name, _) in store.conditions()? {
        held.insert(object(Kind::Condition, &name));
    }

    // What each object names, by how a message says the object.

    let mut naming: Vec<(String, Vec<Target>)> = Vec::new();
    let route_apps: BTreeMap<String, String> = routes
        .iter()
        .filter_map(|route| route.name.clone().map(|name| (name, route.app.clone())))
        .collect();

    for route in &routes {
        naming.push((
            format!("route {} {}", route.host, route.key()),
            self::route(route),
        ));
    }

    for (name, table) in store.hosts()? {
        naming.push((format!("host {name}"), host(&table)));
    }

    for (name, table) in store.rate_limits()? {
        naming.push((format!("rate_limit {name}"), rate_limit(&table)));
    }

    for (name, table) in store.overlays()? {
        naming.push((format!("overlay {name}"), overlay(&table, &route_apps)));
    }

    for release in &releases {
        for group in &release.spec.groups {
            naming.push((
                format!(
                    "{}@{}: worker group {}",
                    release.app, release.id, group.name
                ),
                self::group(&group.client_class, &group.host),
            ));
        }
    }

    // A rollout under way names its two releases; one that has ended is
    // history, and its releases may since have been removed.

    for app in &apps {
        if let Some(rollout) = store.active_rollout(&app.name)? {
            naming.push((
                format!("rollout {} of {}", rollout.id, rollout.app),
                vec![
                    Target::Release(rollout.app.clone(), rollout.candidate.clone()),
                    Target::Release(rollout.app.clone(), rollout.baseline.clone()),
                ],
            ));
        }
    }

    let mut found = Vec::new();

    for (what, names) in naming {
        let missing: BTreeSet<&Target> = names
            .iter()
            .filter(|target| !held.contains(target))
            .collect();

        for target in missing {
            found.push(format!(
                "{what} names {}, which the store does not hold",
                target.describe()
            ));
        }
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(text: &str) -> muru_config::Document {
        muru_config::Document::parse(text).expect("the document parses")
    }

    fn has(targets: &[Target], kind: Kind, name: &str) -> bool {
        targets.contains(&object(kind, name))
    }

    #[test]
    fn a_route_names_all_it_refers_to() {
        let document = named(
            r#"
            [[route]]
            host = "store"
            path = "/api"
            app = "api"
            rate_limits = ["steady"]
            match = { client_class = ["partner"] }
            accept_redirects_from = ["front"]
            auth = { upstream = "decider", path = "/_auth" }
            traffic = { split = { r2 = 10 }, classes = { staff = "r1", robots = "upstream:legacy" } }

            [[route]]
            host = "store"
            path = "/files"
            static = "/srv/files"
            fallback = { upstream = "legacy" }

            [[route]]
            host = "store"
            path = "/account"
            static = "/srv/account"
            fallback = { app = "shop" }
            auth = { app = "gate", path = "/_auth" }
            traffic = { split = { r7 = 5 } }
            "#,
        );
        let first = route(&document.route[0]);

        assert!(has(&first, Kind::Host, "store"));
        assert!(has(&first, Kind::App, "api"));
        assert!(has(&first, Kind::App, "front"));
        assert!(has(&first, Kind::RateLimit, "steady"));
        assert!(has(&first, Kind::ClientClass, "partner"));
        assert!(has(&first, Kind::ClientClass, "staff"));
        assert!(has(&first, Kind::ClientClass, "robots"));
        assert!(has(&first, Kind::Upstream, "decider"));
        assert!(has(&first, Kind::Upstream, "legacy"));
        assert!(first.contains(&Target::Release("api".to_owned(), "r1".to_owned())));
        assert!(first.contains(&Target::Release("api".to_owned(), "r2".to_owned())));

        assert!(has(&route(&document.route[1]), Kind::Upstream, "legacy"));

        let third = route(&document.route[2]);

        assert!(has(&third, Kind::App, "shop"));
        assert!(has(&third, Kind::App, "gate"));
        assert!(third.contains(&Target::Release("shop".to_owned(), "r7".to_owned())));
    }

    #[test]
    fn a_host_a_rate_limit_and_a_group_name_theirs() {
        let document = named(
            r#"
            [host.shop]
            names = ["shop.example.com"]
            rate_limits = ["steady"]

            [rate_limit.partners]
            rate = "10/s"
            match = { client_class = ["partner"] }
            "#,
        );

        assert!(has(
            &host(&document.host["shop"]),
            Kind::RateLimit,
            "steady"
        ));
        assert!(has(
            &rate_limit(&document.rate_limit["partners"]),
            Kind::ClientClass,
            "partner"
        ));

        let group = group(&["partner".to_owned()], &["store".to_owned()]);

        assert!(has(&group, Kind::ClientClass, "partner"));
        assert!(has(&group, Kind::Host, "store"));
    }

    #[test]
    fn an_overlay_names_all_it_refers_to() {
        let document = named(
            r#"
            [overlay.degraded]
            trigger = { condition = "payments-down" }

            [overlay.degraded.apps.front]
            maintenance = true

            [overlay.degraded.routes.api-root]
            rate_limits = ["steady"]
            target = { upstream = "legacy" }

            [overlay.degraded.routes.api-root.traffic]
            pin = "r1"

            [overlay.degraded.routes.api-root.client_routes.partner]
            mode = "read_only"
            traffic = { split = { r3 = 50 } }

            [overlay.degraded.routes.other]
            target = { app = "spare" }

            [overlay.degraded.preconditions]
            release_ready = "api@r2"

            [overlay.degraded.warm_up]
            lead = "30m"
            start_release = "api@r4"
            "#,
        );
        let route_apps = BTreeMap::from([("api-root".to_owned(), "api".to_owned())]);
        let found = overlay(&document.overlay["degraded"], &route_apps);
        let release = |id: &str| Target::Release("api".to_owned(), id.to_owned());

        assert!(has(&found, Kind::Condition, "payments-down"));
        assert!(has(&found, Kind::App, "front"));
        assert!(has(&found, Kind::App, "spare"));
        assert!(has(&found, Kind::RateLimit, "steady"));
        assert!(has(&found, Kind::Upstream, "legacy"));
        assert!(has(&found, Kind::ClientClass, "partner"));
        assert!(found.contains(&Target::NamedRoute("api-root".to_owned())));
        assert!(found.contains(&Target::NamedRoute("other".to_owned())));

        for id in ["r1", "r2", "r3", "r4"] {
            assert!(found.contains(&release(id)), "{id}");
        }
    }
}
