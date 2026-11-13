//! The daemon's probes (design specification section 13.9, issue 622):
//! the questions `muructl serve` answers on `/livez`, `/readyz` and
//! `/startupz`, asked of a daemon kept in a container or under systemd,
//! over the admin socket by `muructl probe` and over HTTP on the metrics
//! address.
//!
//! - **live**: the daemon answers and its edge is alive. It never
//!   depends on a worker being free.
//!
//! - **ready**: the daemon has started and is not shutting down, and
//!   every application with a serving release has at least `min_ready`
//!   workers ready in each group the release needs, an optional group
//!   never holding it back. An application with no serving release,
//!   never deployed or with its deploys failed, does not hold it back.
//!
//! - **startup**: the daemon has finished starting, having adopted the
//!   processes it found or started the current releases afresh, and each
//!   serving release has a worker ready in each group it needs.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::daemon::Info;
use crate::store::Store;

/// What the probes look at.
#[derive(Clone)]
pub struct Probes {
    /// The store, for each application's serving release and its groups.
    pub store: Arc<Store>,
    /// The daemon's state: whether it has started or is stopping, its
    /// edge, and the workers each release runs.
    pub info: Arc<Info>,
}

impl Probes {
    /// Answer `probe` (`live`, `ready` or `startup`), narrowed to `app`
    /// when it is not empty: why not, when the answer is no.
    pub fn answer(&self, probe: &str, app: &str, min_ready: u32) -> Result<(), String> {
        match probe {
            "live" => self.live(),
            "ready" => self.ready(app, min_ready.max(1)),
            "startup" => self.startup(app),
            other => Err(format!("{other:?} is not a probe: live, ready or startup")),
        }
    }

    fn live(&self) -> Result<(), String> {
        let edge = self.info.edge_pid.load(Ordering::Relaxed);

        if edge == 0 || !crate::host::alive(edge) {
            return Err("the edge is not running".to_owned());
        }

        Ok(())
    }

    fn ready(&self, app: &str, min_ready: u32) -> Result<(), String> {
        if self.info.stopping.load(Ordering::Relaxed) {
            return Err("the daemon is stopping".to_owned());
        }

        if !self.info.started.load(Ordering::Relaxed) {
            return Err("the daemon is starting".to_owned());
        }

        self.serving(app, min_ready)
    }

    fn startup(&self, app: &str) -> Result<(), String> {
        if !self.info.started.load(Ordering::Relaxed) {
            return Err("the daemon is starting".to_owned());
        }

        self.serving(app, 1)
    }

    /// Whether each serving release, of `app` when it is named, has
    /// `min_ready` workers ready in each group it needs.
    fn serving(&self, app: &str, min_ready: u32) -> Result<(), String> {
        let apps = self.store.apps().map_err(|error| error.to_string())?;

        if !app.is_empty() && !apps.iter().any(|record| record.name == app) {
            return Err(format!("there is no application {app}"));
        }

        let live = self.info.live.lock().expect("live releases poisoned");

        for record in apps
            .iter()
            .filter(|record| app.is_empty() || record.name == app)
        {
            let Some(id) = &record.current else {
                continue;
            };

            let Ok(release) = self.store.release(&record.name, id) else {
                continue;
            };

            let workers = live
                .get(&(record.name.clone(), id.clone()))
                .map(|running| running.workers.as_slice())
                .unwrap_or_default();
            let required = std::iter::once(muru_edge::routing::DEFAULT_GROUP).chain(
                release
                    .spec
                    .groups
                    .iter()
                    .filter(|group| !group.optional)
                    .map(|group| group.name.as_str()),
            );

            for group in required {
                let ready = workers
                    .iter()
                    .filter(|worker| worker.group == group && worker.state == "ready")
                    .count() as u32;

                if ready < min_ready {
                    return Err(format!(
                        "{}@{id}: group {group} has {ready} of {min_ready} workers ready",
                        record.name
                    ));
                }
            }
        }

        Ok(())
    }
}
