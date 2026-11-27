//! The daemon's side of previews (design specification section 4.22, ADR
//! 0084): which release a preview is of, its secret, and the one-time
//! entry links that give a browser the preview cookie. A release's
//! secret is kept in the store and a detour's in memory, and neither
//! leaves the daemon but in the answer to a preview that was asked for
//! and in the snapshot its own edge is given.

use std::time::Duration;

use muru_proto::admin::v1 as admin;
use muru_proto::edge::v1 as pb;

use super::Daemon;
use crate::detours;

/// How long an entry link lasts when nothing is asked.
pub const ENTRY_TTL: Duration = Duration::from_secs(60);

/// The longest an entry link may be asked to last.
pub const LONGEST_ENTRY_TTL: Duration = Duration::from_secs(3600);

impl Daemon {
    /// The value that previews a release, `RELEASE.SECRET`, for the
    /// daemon's own probes through its edge.
    pub(super) fn preview_value(&self, app: &str, release: &str) -> Result<String, String> {
        let secret = match self.detours.get(&(app.to_owned(), release.to_owned())) {
            Some(detour) => detour.secret.clone(),
            None => self
                .store
                .preview_secret(app, release)
                .map_err(|error| format!("{app}@{release} cannot be previewed: {error}"))?,
        };

        Ok(muru_edge::preview::value(release, &secret))
    }

    /// The release a preview request means. One it names is taken as it
    /// is, `detour` standing for the application's one detour; with none
    /// named it is the application's open detour, else its one held
    /// release, and anything else is refused with what there is.
    fn preview_target(&self, app: &str, release: &str) -> Result<String, String> {
        let open: Vec<&str> = self
            .detours
            .keys()
            .filter(|(of, _)| of == app)
            .map(|(_, release)| release.as_str())
            .collect();
        let named = |releases: &[&str]| {
            releases
                .iter()
                .map(|release| format!("{app}@{release}"))
                .collect::<Vec<_>>()
                .join(", ")
        };

        if !release.is_empty() && release != detours::ALIAS {
            return Ok(release.to_owned());
        }

        match open.as_slice() {
            [only] => return Ok((*only).to_owned()),
            [] if release == detours::ALIAS => {
                return Err(format!("application {app} has no detour open"));
            }
            [] => {}
            several => {
                return Err(format!(
                    "{app} has {} detours open, so name the one to preview: {}",
                    several.len(),
                    named(several)
                ));
            }
        }

        self.store
            .app(app)
            .map_err(|_| format!("there is no application {app} on this daemon"))?;
        let mut held: Vec<&str> = self
            .running
            .iter()
            .filter(|((of, _), running)| of == app && running.held)
            .map(|((_, release), _)| release.as_str())
            .collect();
        held.sort_unstable();

        match held.as_slice() {
            [only] => Ok((*only).to_owned()),
            [] => {
                let routed: Vec<&str> = self
                    .routed
                    .iter()
                    .filter(|(of, _)| of == app)
                    .map(|(_, release)| release.as_str())
                    .collect();

                Err(if routed.is_empty() {
                    format!("{app} has no detour open and no release that takes requests")
                } else {
                    format!(
                        "{app} has no detour open and no held release, so name the release to \
                         preview: {}",
                        named(&routed)
                    )
                })
            }
            several => Err(format!(
                "{app} has {} held releases, so name the one to preview: {}",
                several.len(),
                named(several)
            )),
        }
    }

    /// Mint entry links to a release's preview (section 4.22): one for
    /// the application's best address, as `muructl open` ranks them, one
    /// for a host asked for, or one for each. The edge is given each
    /// token's hash; the token itself goes only to the caller, with the
    /// preview value the header carries.
    pub(super) async fn preview_release(
        &mut self,
        request: admin::PreviewReleaseRequest,
    ) -> Result<admin::ReleasePreview, String> {
        let app = request.app.clone();
        let release = self.preview_target(&app, &request.release)?;
        let key = (app.clone(), release.clone());
        let subject = format!("{app}@{release}");

        let valid_for = match request.valid_for_ms {
            0 => ENTRY_TTL,
            asked => Duration::from_millis(asked),
        };

        if valid_for > LONGEST_ENTRY_TTL {
            return Err(format!(
                "a preview link lasts at most an hour, and {} was asked for",
                muru_config::format_duration(valid_for)
            ));
        }

        // Only what the edge routes to can be previewed: a detour, the
        // serving release, one a policy names and one that is held.

        if !self.detours.contains_key(&key) && !self.routed.contains(&key) {
            return Err(match self.store.release(&app, &release) {
                Ok(stored) => format!(
                    "{subject} is {} and takes no requests, so it cannot be previewed: the \
                     serving release, a release a traffic policy names and one deployed with \
                     --hold can be",
                    stored.state.as_str()
                ),
                Err(_) => format!("there is no release {subject} on this daemon"),
            });
        }

        // A new secret is in force at the edge before anything is
        // answered, so what is answered works and what came before does
        // not.

        if request.new_secret {
            match self.detours.get_mut(&key) {
                Some(detour) => detour.secret = detours::secret()?,
                None => {
                    self.store
                        .renew_preview_secret(&app, &release)
                        .map_err(|error| error.to_string())?;
                }
            }

            self.push_snapshot()
                .await
                .map_err(|error| format!("the edge did not take the new secret: {error:#}"))?;
            self.events.emit(
                "release.preview_secret_changed",
                &subject,
                "notice",
                "a new preview secret: links, cookies and headers made before no longer preview it",
            );
        }

        let header = self.preview_value(&app, &release)?;
        let routes = self.store.routes().unwrap_or_default();
        let hosts = self.store.hosts().unwrap_or_default();
        let listeners = self
            .info
            .listeners
            .read()
            .expect("listeners poisoned")
            .clone();
        let name = (!request.name.is_empty()).then_some(request.name.as_str());
        let candidates = crate::urls::for_app(&app, &routes, &hosts, &listeners, name);
        let total = candidates.len();

        let chosen: Vec<crate::urls::Candidate> = if request.all {
            candidates
        } else if !request.host.is_empty() {
            candidates
                .into_iter()
                .filter(|candidate| {
                    candidate.name == request.host || candidate.host == request.host
                })
                .take(1)
                .collect()
        } else {
            candidates.into_iter().take(1).collect()
        };

        // Issue 908: the header needs no address, so an application with
        // none still has it, and the caller is told why there is no link.

        if chosen.is_empty() {
            return Ok(admin::ReleasePreview {
                links: Vec::new(),
                header,
                others: u32::try_from(total).unwrap_or(u32::MAX),
                unreachable: if total == 0 {
                    crate::urls::no_address(&app, &routes, &hosts, name)
                } else {
                    format!(
                        "{app} has no address on {}; muructl preview --list gives each",
                        request.host
                    )
                },
                app,
                release,
                valid_for_ms: 0,
            });
        }

        let mut links = Vec::new();
        let mut tokens = Vec::new();

        for candidate in &chosen {
            let token = detours::secret()?;
            tokens.push(pb::PreviewToken {
                sha256: muru_edge::preview::hash(&token).to_vec(),
                app: app.clone(),
                release: release.clone(),
                path: candidate.path.clone(),
                ttl_ms: valid_for.as_millis() as u64,
            });
            links.push(admin::PreviewLink {
                url: crate::urls::url(
                    &candidate.scheme,
                    &candidate.name,
                    candidate.port,
                    &format!("{}{token}", muru_edge::preview::PREFIX),
                ),
                address: candidate.url.clone(),
                name: candidate.name.clone(),
                bound: candidate.bound.to_string(),
            });
        }

        self.edge
            .add_preview_tokens(tokens)
            .await
            .map_err(|error| format!("the edge did not take the entry links: {error:#}"))?;

        Ok(admin::ReleasePreview {
            others: u32::try_from(total.saturating_sub(links.len())).unwrap_or(u32::MAX),
            links,
            header,
            unreachable: String::new(),
            app,
            release,
            valid_for_ms: valid_for.as_millis() as u64,
        })
    }
}
