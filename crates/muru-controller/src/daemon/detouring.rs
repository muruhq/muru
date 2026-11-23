//! The daemon's side of detours (design specification section 9.13, ADR
//! 0075): opening and closing them, checking that their runs are alive,
//! and what each adds to the edge's snapshot. A detour lives in the
//! daemon's memory alone; nothing of it reaches the store.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use muru_proto::admin::v1 as admin;
use muru_proto::edge::v1 as pb;

use super::Daemon;
use crate::detours::{self, Detour, Opening, Traffic};

/// How often the detours' runs are looked for in the instance registry.
const CHECK_EVERY: Duration = Duration::from_secs(5);

impl Daemon {
    /// Open a detour for a run: check what it asks, name its release,
    /// make its secret and route to it. Answered with the stream's first
    /// message and the opening's serial.
    pub(super) async fn open_detour(
        &mut self,
        opening: Opening,
        updates: detours::Updates,
    ) -> Result<(admin::DetourOpened, u64), String> {
        if let Some(refusal) = detours::opening_refusal(
            self.detours_enabled,
            self.locations.mode,
            &self.locations.config.path,
        ) {
            return Err(refusal);
        }

        let app = opening.app.clone();
        let record = self
            .store
            .app(&app)
            .map_err(|_| format!("there is no application {app} on this daemon to detour"))?;

        if !self.serving.contains_key(&app) {
            return Err(format!(
                "application {app} has no serving release for the detour to stand beside; \
                 deploy it first"
            ));
        }

        // A run that opens its detour again, after its stream dropped,
        // replaces the detour it had, keeping its release's name.

        if !opening.instance_id.is_empty()
            && let Some(serial) = self
                .detours
                .values()
                .find(|detour| {
                    detour.app() == app && detour.opening.instance_id == opening.instance_id
                })
                .map(|detour| detour.serial)
        {
            self.remove_detour(serial, "its run opened it again");
        }

        let release = detours::release_name(&opening.instance_id, |name| {
            self.detours.contains_key(&(app.clone(), name.to_owned()))
        });

        if let Some(refusal) = self.traffic_refusal(&app, &release, &opening.traffic) {
            return Err(refusal);
        }

        let secret = detours::secret()?;
        self.detour_serial += 1;
        let serial = self.detour_serial;

        let detour = Detour {
            opening,
            release: release.clone(),
            secret,
            opened_unix_ms: crate::model::now_unix_ms() as i64,
            serial,
            updates,
        };

        let data = crate::places::app_data(&self.locations.state.path, &record);
        let opened = admin::DetourOpened {
            detour: Some(detour.message()),
            hosts: self.detour_hosts(&app),
            data_dir: data.data.display().to_string(),
            media_dir: data.media.display().to_string(),
            uploads_dir: data.uploads.display().to_string(),
        };
        let describe = detour.describe();

        // The run hears what was opened before anything else on its
        // stream, which is new and has room.

        let _ = detour.updates.try_send(Ok(admin::DetourMessage {
            what: Some(admin::detour_message::What::Opened(opened.clone())),
        }));
        self.detours.insert((app.clone(), release.clone()), detour);

        // The edge must take the snapshot that routes to the detour, or
        // the detour is not open.

        if let Err(error) = self.push_snapshot().await {
            self.detours.remove(&(app.clone(), release.clone()));
            let _ = self.push_snapshot().await;
            return Err(format!("the edge refused the detour's routing: {error}"));
        }

        self.publish_detours();
        let subject = format!("{app}@{release}");
        self.events.emit(
            "detour.opened",
            &subject,
            "info",
            &format!("detour {subject} opened by the development run with {describe}"),
        );

        Ok((opened, serial))
    }

    /// A detour's stream ended: remove it and route without it. A serial
    /// no detour has, one closed already, is nothing to do.
    pub(super) async fn close_detour(&mut self, serial: u64, reason: &str) {
        if self.remove_detour(serial, reason) {
            if let Err(error) = self.push_snapshot().await {
                tracing::warn!(%error, "the snapshot without a closed detour was refused");
            }

            self.publish_detours();
        }
    }

    /// Take a detour out of the daemon's keeping, saying why on its
    /// stream and in an event; the snapshot is the caller's to push.
    /// False when no detour has the serial.
    fn remove_detour(&mut self, serial: u64, reason: &str) -> bool {
        let Some(key) = self
            .detours
            .iter()
            .find(|(_, detour)| detour.serial == serial)
            .map(|(key, _)| key.clone())
        else {
            return false;
        };

        let Some(detour) = self.detours.remove(&key) else {
            return false;
        };

        // The run may be gone already, so the last message is offered
        // and not waited for.

        let _ = detour.updates.try_send(Ok(admin::DetourMessage {
            what: Some(admin::detour_message::What::Closing(reason.to_owned())),
        }));

        let subject = format!("{}@{}", detour.app(), detour.release);
        self.events.emit(
            "detour.closed",
            &subject,
            "info",
            &format!(
                "detour {subject} of the development run with {} closed: {reason}",
                detour.describe()
            ),
        );
        true
    }

    /// Close every detour, as the daemon stops: each run hears why, and
    /// the edge, which may go on serving for the next daemon, is left
    /// routing to none of them.
    pub(super) async fn close_detours(&mut self, reason: &str) {
        let serials: Vec<u64> = self.detours.values().map(|detour| detour.serial).collect();

        if serials.is_empty() {
            return;
        }

        for serial in serials {
            self.remove_detour(serial, reason);
        }

        if let Err(error) = self.push_snapshot().await {
            tracing::warn!(%error, "the snapshot without the detours was refused");
        }

        self.publish_detours();
    }

    /// The registry is a second check that a detour's run is alive (ADR
    /// 0069): a detour whose run's entry is gone or no longer locked is
    /// closed, and so is one whose application has gone or no longer
    /// serves. Looked at every few seconds.
    pub(super) async fn check_detours(&mut self) {
        if self.detours.is_empty() || self.detours_checked.elapsed() < CHECK_EVERY {
            return;
        }

        self.detours_checked = Instant::now();
        let registry = crate::registry::user_registry();
        let mut gone = Vec::new();

        for detour in self.detours.values() {
            let run_gone = detour
                .opening
                .instance_id
                .parse::<crate::registry::InstanceId>()
                .is_ok_and(|id| !crate::registry::entry_is_live(&registry, id));

            let why = if self.store.app(detour.app()).is_err() {
                Some("its application was removed")
            } else if !self.serving.contains_key(detour.app()) {
                Some("its application has no serving release")
            } else if run_gone {
                Some("its run's entry in the instance registry is gone")
            } else {
                None
            };

            if let Some(why) = why {
                gone.push((detour.serial, why));
            }
        }

        for (serial, why) in gone {
            self.close_detour(serial, why).await;
        }
    }

    /// Change what a detour takes (section 9.13): the overlay it owns
    /// changes with it, its run hears of it, and `detour.traffic` records
    /// the old choice and the new.
    pub(super) async fn set_detour_traffic(
        &mut self,
        app: &str,
        release: &str,
        traffic: Traffic,
    ) -> Result<admin::Detour, String> {
        let key = (app.to_owned(), release.to_owned());

        if !self.detours.contains_key(&key) {
            return Err(format!("{app}@{release} is not an open detour"));
        }

        if let Some(refusal) = self.traffic_refusal(app, release, &traffic) {
            return Err(refusal);
        }

        let Some(detour) = self.detours.get_mut(&key) else {
            return Err(format!("{app}@{release} is not an open detour"));
        };

        let before = std::mem::replace(&mut detour.opening.traffic, traffic.clone());

        if let Err(error) = self.push_snapshot().await {
            if let Some(detour) = self.detours.get_mut(&key) {
                detour.opening.traffic = before;
            }

            let _ = self.push_snapshot().await;
            return Err(format!("the edge refused the detour's traffic: {error}"));
        }

        let Some(detour) = self.detours.get(&key) else {
            return Err(format!("{app}@{release} closed as its traffic changed"));
        };

        let message = detour.message();
        let _ = detour.updates.try_send(Ok(admin::DetourMessage {
            what: Some(admin::detour_message::What::Changed(message.clone())),
        }));
        self.publish_detours();

        let subject = format!("{app}@{release}");
        self.events.emit(
            "detour.traffic",
            &subject,
            "info",
            &format!(
                "detour {subject} takes {} now, {} before",
                traffic.word(),
                before.word()
            ),
        );

        Ok(message)
    }

    /// What every open detour asks of its application's routes.
    pub(super) fn detour_takings(&self) -> Vec<detours::Taking<'_>> {
        self.detours
            .values()
            .map(|detour| detours::Taking {
                app: detour.app(),
                release: &detour.release,
                traffic: &detour.opening.traffic,
            })
            .collect()
    }

    /// Why a detour of `app` cannot take `traffic` now: a rollout under
    /// way, another detour's claim, or a client class the daemon does
    /// not define.
    fn traffic_refusal(&self, app: &str, release: &str, traffic: &Traffic) -> Option<String> {
        let rollout = self
            .store
            .rollouts(Some(app))
            .unwrap_or_default()
            .iter()
            .any(|rollout| rollout.state.is_active());
        let classes: Vec<String> = self
            .clients
            .classes_message
            .iter()
            .map(|class| class.name.clone())
            .collect();
        let others: Vec<detours::Taking<'_>> = self
            .detour_takings()
            .into_iter()
            .filter(|other| other.app == app && other.release != release)
            .collect();

        detours::traffic_refusal(
            detours::Taking {
                app,
                release,
                traffic,
            },
            &others,
            rollout,
            &classes,
        )
    }

    /// Publish the open detours for the API.
    pub(super) fn publish_detours(&self) {
        let detours = self.detours.values().map(Detour::message).collect();
        *self.info.detours.lock().expect("detours poisoned") = detours;
    }

    /// The application's host names, patterns left out, as its URLs give
    /// them: what the run's toolbar accepts beside loopback names.
    fn detour_hosts(&self, app: &str) -> Vec<String> {
        let routes = self.store.routes().unwrap_or_default();
        let hosts = self.store.hosts().unwrap_or_default();
        let listeners = self
            .info
            .listeners
            .read()
            .expect("listeners poisoned")
            .clone();
        let mut names = BTreeSet::new();

        crate::urls::for_app(app, &routes, &hosts, &listeners)
            .into_iter()
            .map(|candidate| candidate.name)
            .filter(|name| names.insert(name.clone()))
            .collect()
    }

    /// What the detours add to the snapshot: a release of each
    /// application whose server is the run's socket.
    pub(super) fn detour_apps(&self, maintained: &BTreeSet<String>) -> Vec<pb::AppWorkers> {
        self.detours
            .values()
            .map(|detour| {
                let app = detour.app().to_owned();
                let access_log = self
                    .store
                    .app(&app)
                    .ok()
                    .filter(|record| !record.access_log.is_empty())
                    .map(|record| record.access_log.choice().to_message());

                pb::AppWorkers {
                    maintenance: Some(super::maintenance_message(maintained.contains(&app), None)),
                    server: Some(pb::Upstream {
                        name: format!("{app}@{}", detour.release),
                        addresses: vec![detour.address()],
                        protocol: "http1".to_owned(),
                        trust_muru_headers: true,
                        detour: true,
                        ..Default::default()
                    }),
                    release: detour.release.clone(),
                    access_log,
                    app,
                    ..Default::default()
                }
            })
            .collect()
    }
}
