//! Named deployments (design specification section 10.13, ADR 0080):
//! which deployment owns each object an apply creates, what the files of
//! an apply declare for each, and how far a prune reaches.
//!
//! A deployment is a named set of objects applied, pruned and removed
//! together. Files name theirs with a `deployment` key, and files without
//! one belong to `default`, as does what commands make. Names stay
//! node-wide: ownership says who may change a thing, not who may refer to
//! it.

use std::collections::{BTreeMap, BTreeSet};

use muru_config::Document;

use crate::store::Store;

/// The deployment of files with no `deployment` key and of what commands
/// make. It always exists.
pub const DEFAULT: &str = "default";

/// The kinds of object a deployment owns. An application's releases and
/// settings go with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// An application.
    App,
    /// A virtual host.
    Host,
    /// A route, by its host and its key within it.
    Route,
    /// An upstream.
    Upstream,
    /// An overlay.
    Overlay,
    /// A client class.
    ClientClass,
    /// A rate limit.
    RateLimit,
    /// A condition.
    Condition,
}

impl Kind {
    /// Every kind.
    pub const ALL: [Kind; 8] = [
        Kind::App,
        Kind::Host,
        Kind::Route,
        Kind::Upstream,
        Kind::Overlay,
        Kind::ClientClass,
        Kind::RateLimit,
        Kind::Condition,
    ];

    /// The kind as the store keeps it and a message names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::App => "app",
            Kind::Host => "host",
            Kind::Route => "route",
            Kind::Upstream => "upstream",
            Kind::Overlay => "overlay",
            Kind::ClientClass => "client_class",
            Kind::RateLimit => "rate_limit",
            Kind::Condition => "condition",
        }
    }

    /// The kind the store kept, none for one this version does not know.
    pub fn parse(text: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

/// A route as ownership names it: its host and its key within the host.
pub fn route_key(host: &str, key: &str) -> String {
    format!("{host} {key}")
}

/// Which deployment owns each object, by kind and name. An object that is
/// not here belongs to `default`.
pub type Owners = BTreeMap<(Kind, String), String>;

/// How far the removals of an apply reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Prune {
    /// Nothing is removed.
    #[default]
    No,
    /// What the deployments the files name own and the files no longer
    /// declare.
    Files,
    /// The same in every deployment, those the files never name among
    /// them, so the files are the whole of what the daemon serves.
    All,
}

impl Prune {
    /// Whether anything is removed.
    pub fn any(self) -> bool {
        self != Prune::No
    }
}

/// What the files of one apply declare: every table as one document, the
/// deployment of each object in it, and the deployments the files name.
#[derive(Debug, Clone, Default)]
pub struct Declared {
    /// Every file's tables together, without the `deployment` keys.
    pub document: Document,
    /// The deployment each declared object belongs to, `default` for one
    /// of a file with no key.
    pub owners: Owners,
    /// The deployments the files name, `default` among them when a file
    /// has no key.
    pub named: BTreeSet<String>,
}

impl Declared {
    /// One document, all of it `default`'s: what an apply was before
    /// deployments had names, and still is for files with no key.
    pub fn of_default(document: Document) -> Declared {
        fold(vec![document]).unwrap_or_default()
    }

    /// The deployment a declared object belongs to.
    pub fn owner(&self, kind: Kind, key: &str) -> &str {
        self.owners
            .get(&(kind, key.to_owned()))
            .map_or(DEFAULT, String::as_str)
    }
}

/// The objects a document declares, by kind and name.
pub fn objects(document: &Document) -> Vec<(Kind, String)> {
    let mut found = Vec::new();
    found.extend(document.app.keys().map(|name| (Kind::App, name.clone())));
    found.extend(document.host.keys().map(|name| (Kind::Host, name.clone())));
    found.extend(
        document
            .route
            .iter()
            .map(|route| (Kind::Route, route_key(&route.host, &route.key()))),
    );
    found.extend(
        document
            .upstream
            .keys()
            .map(|name| (Kind::Upstream, name.clone())),
    );
    found.extend(
        document
            .overlay
            .keys()
            .map(|name| (Kind::Overlay, name.clone())),
    );
    found.extend(
        document
            .client_class
            .iter()
            .map(|class| (Kind::ClientClass, class.name.clone())),
    );
    found.extend(
        document
            .rate_limit
            .keys()
            .map(|name| (Kind::RateLimit, name.clone())),
    );
    found.extend(
        document
            .condition
            .keys()
            .map(|name| (Kind::Condition, name.clone())),
    );
    found
}

/// Fold the documents of an apply, one to a deployment, into what they
/// declare together. An object two deployments declare is refused, as is
/// a deployment name that is not a name.
pub fn fold(documents: Vec<Document>) -> Result<Declared, String> {
    let mut declared = Declared::default();
    let alone = documents.len() == 1;

    for mut document in documents {
        let name = document
            .deployment
            .take()
            .unwrap_or_else(|| DEFAULT.to_owned());

        if !crate::model::valid_name(&name) {
            return Err(format!(
                "{name:?} is not a valid deployment name: lowercase letters, digits and hyphens, \
                 up to 63 characters"
            ));
        }

        for (kind, key) in objects(&document) {
            // An application's releases may be in several files of its
            // deployment, so the same deployment naming it again is no
            // clash; the document's own merge refuses what is.

            if let Some(other) = declared.owners.get(&(kind, key.clone()))
                && *other != name
            {
                return Err(format!(
                    "{} {key} is declared in the deployments {other} and {name}; an object \
                     belongs to one",
                    kind.as_str()
                ));
            }

            declared.owners.insert((kind, key), name.clone());
        }

        declared.named.insert(name);

        // Folding keeps the tables of applied files. A table of the
        // daemon's own, which an apply refuses, is refused here, where
        // it is still there to see.

        if let Some(table) = document.daemon_tables().first()
            && !alone
        {
            return Err(format!(
                "{table} is part of the daemon's own configuration, which muructl apply does \
                 not change: put it in the daemon's configuration file and run muructl daemon \
                 reload"
            ));
        }

        if alone {
            declared.document = document;
        } else {
            declared.document.merge(document)?;
        }
    }

    if declared.named.is_empty() {
        declared.named.insert(DEFAULT.to_owned());
    }

    Ok(declared)
}

/// The objects the store holds, by kind and name.
pub fn stored_objects(store: &Store) -> BTreeSet<(Kind, String)> {
    let mut found = BTreeSet::new();
    found.extend(
        store
            .apps()
            .unwrap_or_default()
            .into_iter()
            .map(|app| (Kind::App, app.name)),
    );
    found.extend(
        store
            .hosts()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, _)| (Kind::Host, name)),
    );
    found.extend(
        store
            .routes()
            .unwrap_or_default()
            .into_iter()
            .map(|route| (Kind::Route, route_key(&route.host, &route.key()))),
    );
    found.extend(
        store
            .upstreams()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, _)| (Kind::Upstream, name)),
    );
    found.extend(
        store
            .overlays()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, _)| (Kind::Overlay, name)),
    );
    found.extend(
        store
            .client_classes()
            .unwrap_or_default()
            .into_iter()
            .map(|(_, class)| (Kind::ClientClass, class.name)),
    );
    found.extend(
        store
            .rate_limits()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, _)| (Kind::RateLimit, name)),
    );
    found.extend(
        store
            .conditions()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, _)| (Kind::Condition, name)),
    );
    found
}

/// Who owns what the store holds: the named deployment recorded for each
/// object that is still there. An object with no record is `default`'s.
pub fn stored(store: &Store) -> Owners {
    let there = stored_objects(store);

    store
        .ownership()
        .unwrap_or_default()
        .into_iter()
        .filter(|(object, _)| there.contains(object))
        .collect()
}

/// A deployment and what it holds, for `muructl deployment list`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// The applications it owns.
    pub apps: Vec<String>,
    /// How many hosts it owns.
    pub hosts: usize,
    /// How many routes.
    pub routes: usize,
    /// How many of anything else: upstreams, overlays, classes, limits
    /// and conditions.
    pub others: usize,
}

/// Every deployment with what it holds, by name. `default` is always
/// among them.
pub fn list(store: &Store) -> BTreeMap<String, Summary> {
    let owners = stored(store);
    let mut found: BTreeMap<String, Summary> = BTreeMap::new();
    found.entry(DEFAULT.to_owned()).or_default();

    for (kind, key) in stored_objects(store) {
        let owner = owners
            .get(&(kind, key.clone()))
            .cloned()
            .unwrap_or_else(|| DEFAULT.to_owned());
        let summary = found.entry(owner).or_default();

        match kind {
            Kind::App => summary.apps.push(key),
            Kind::Host => summary.hosts += 1,
            Kind::Route => summary.routes += 1,
            _ => summary.others += 1,
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> Document {
        Document::parse(text).unwrap()
    }

    #[test]
    fn files_are_folded_a_deployment_at_a_time() {
        let shop = document(
            "deployment = 'shop'\n[app.shop.release.r1]\nsource = '/srv/shop'\n\
             [host.shop]\nnames = ['shop.test']\n[[route]]\nhost = 'shop'\napp = 'shop'\n",
        );
        let plain = document("[app.blog.release.r1]\nsource = '/srv/blog'\n");
        let declared = fold(vec![shop, plain]).unwrap();

        assert_eq!(declared.document.app.len(), 2);
        assert_eq!(declared.document.deployment, None);
        assert_eq!(declared.owner(Kind::App, "shop"), "shop");
        assert_eq!(declared.owner(Kind::Route, "shop /"), "shop");
        assert_eq!(declared.owner(Kind::App, "blog"), DEFAULT);
        assert_eq!(
            declared
                .named
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["default", "shop"]
        );

        // An object belongs to one deployment, and a name is a name.

        let again = document("deployment = 'store'\n[host.shop]\nnames = ['store.test']\n");
        let shop = document("deployment = 'shop'\n[host.shop]\nnames = ['shop.test']\n");
        assert_eq!(
            fold(vec![shop, again]).unwrap_err(),
            "host shop is declared in the deployments shop and store; an object belongs to one"
        );
        assert!(
            fold(vec![document("deployment = 'Not A Name'\n")])
                .unwrap_err()
                .contains("is not a valid deployment name")
        );
    }
}
