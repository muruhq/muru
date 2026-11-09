//! Which releases may be removed, and what goes with one that is (design
//! specification section 9.1). What is decided here is free of the daemon
//! so that the rules can be tested directly: the daemon supplies what it
//! knows about each release and checks a release again just before it
//! drops the record.

use std::path::{Path, PathBuf};

use crate::disk::{DiskUse, Measure};
use crate::locations::Locations;
use crate::model::ReleaseState;
use crate::provision::Provisioning;

/// What a decision needs to know about one release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The release id.
    pub id: String,
    /// Where it is.
    pub state: ReleaseState,
    /// When it was created, Unix milliseconds.
    pub created_unix_ms: u64,
    /// Whether applied files declare it.
    pub declared: bool,
    /// Whether a route's traffic names it or a rollout under way has it
    /// as its candidate.
    pub referenced: bool,
}

/// Why a release stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    /// It serves the application's traffic.
    Serving,
    /// A rollback restores it.
    Rollback,
    /// It has processes or is on its way to having them.
    InUse(ReleaseState),
    /// A route's traffic or a rollout under way uses it.
    Referenced,
    /// Applied files declare it, and it was not forced.
    Declared,
    /// Within the number of newest releases asked for.
    WithinKeep,
    /// Failed, and not yet past the cutoff that lets a failed release go.
    FailedRecent,
}

impl Keep {
    /// The short name in the API's answer.
    pub fn as_str(self) -> &'static str {
        match self {
            Keep::Serving => "serving",
            Keep::Rollback => "rollback",
            Keep::InUse(_) => "in_use",
            Keep::Referenced => "referenced",
            Keep::Declared => "declared",
            Keep::WithinKeep => "keep",
            Keep::FailedRecent => "failed",
        }
    }

    /// What it means, for a person.
    pub fn describe(self) -> String {
        match self {
            Keep::Serving => "it serves the application's traffic".to_owned(),
            Keep::Rollback => "it is the release a rollback restores".to_owned(),
            Keep::InUse(state) => format!("it is {state}"),
            Keep::Referenced => "a route's traffic or a rollout under way uses it".to_owned(),
            Keep::Declared => {
                "applied files declare it, so the next apply creates it again; --force takes it"
                    .to_owned()
            }
            Keep::WithinKeep => "it is among the newest kept".to_owned(),
            Keep::FailedRecent => {
                "it failed, and a failed release stays until both the serving and the rollback \
                 release are newer"
                    .to_owned()
            }
        }
    }
}

/// Why a release goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Older than the newest releases kept.
    BeyondKeep,
    /// Failed, and older than the serving and the rollback release.
    Failed,
    /// Named on its own.
    Named,
    /// Its whole application is going.
    Application,
}

impl Reason {
    /// The short name in the API's answer.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::BeyondKeep => "keep",
            Reason::Failed => "failed",
            Reason::Named => "named",
            Reason::Application => "application",
        }
    }

    /// The message of the `release.removed` event.
    pub fn event_message(self) -> &'static str {
        match self {
            Reason::BeyondKeep => "pruned: older than the newest releases kept",
            Reason::Failed => "pruned: failed, and older than the serving and rollback releases",
            Reason::Named => "removed on request",
            Reason::Application => "its application was removed",
        }
    }
}

/// Why a release is protected from removal, whoever asks; none when it
/// may go. `current` and `previous` are the application's serving and
/// rollback releases.
pub fn protection(
    current: Option<&str>,
    previous: Option<&str>,
    release: &Candidate,
) -> Option<Keep> {
    if current == Some(release.id.as_str()) {
        Some(Keep::Serving)
    } else if previous == Some(release.id.as_str()) {
        Some(Keep::Rollback)
    } else if !release.state.is_final() {
        Some(Keep::InUse(release.state))
    } else if release.referenced {
        Some(Keep::Referenced)
    } else {
        None
    }
}

/// What a prune of one application decides.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Decision {
    /// What goes, oldest first.
    pub removed: Vec<(String, Reason)>,
    /// What stays, and why.
    pub kept: Vec<(String, Keep)>,
}

/// Decide what a prune of one application removes: the rules of section
/// 9.1.
///
/// Protected releases always stay. A failed release stays until both the
/// serving and the rollback release were created after it, and goes
/// whatever `keep` is once they were; an application without both keeps
/// its failed releases. Of the others, the `keep` newest stay. A release
/// applied files declare stays unless `force` is set.
pub fn decide_prune(
    current: Option<&str>,
    previous: Option<&str>,
    candidates: &[Candidate],
    keep: u32,
    force: bool,
) -> Decision {
    let created = |id: Option<&str>| {
        id.and_then(|id| candidates.iter().find(|candidate| candidate.id == id))
            .map(|candidate| candidate.created_unix_ms)
    };

    // A rollback swaps the two, so the rollback release can be the newer.
    // The older of the pair is the cutoff.

    let cutoff = created(current)
        .zip(created(previous))
        .map(|(a, b)| a.min(b));

    let mut ordered: Vec<&Candidate> = candidates.iter().collect();
    ordered.sort_by(|a, b| {
        (a.created_unix_ms, &a.id)
            .cmp(&(b.created_unix_ms, &b.id))
            .reverse()
    });

    let mut decision = Decision::default();
    let mut left_to_keep = keep;

    for candidate in ordered {
        let id = candidate.id.clone();

        if let Some(why) = protection(current, previous, candidate) {
            decision.kept.push((id, why));
        } else if candidate.declared && !force {
            decision.kept.push((id, Keep::Declared));
        } else if candidate.state == ReleaseState::Failed {
            if cutoff.is_some_and(|cutoff| candidate.created_unix_ms < cutoff) {
                decision.removed.push((id, Reason::Failed));
            } else {
                decision.kept.push((id, Keep::FailedRecent));
            }
        } else if left_to_keep > 0 {
            left_to_keep -= 1;
            decision.kept.push((id, Keep::WithinKeep));
        } else {
            decision.removed.push((id, Reason::BeyondKeep));
        }
    }

    decision.removed.reverse();
    decision
}

/// A release's log and its archives, by path. Rotated archives are named
/// for the log with a stamp and a compression suffix.
pub fn release_logs(locations: &Locations, app: &str, id: &str) -> Vec<PathBuf> {
    let log = locations.release_log(app, id);
    let Some(directory) = log.parent().map(Path::to_path_buf) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let wanted = format!("{id}.log");

    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == wanted || crate::log_files::archive_base(&name) == Some(wanted.as_str())
        })
        .map(|entry| entry.path())
        .collect()
}

/// Everything of a release's that goes with it: its environment, bytecode,
/// run directory and, unless `keep_logs`, its log and archives. Its source
/// directory is never among them: Muru points at it and does not own it.
pub fn release_paths(locations: &Locations, app: &str, id: &str, keep_logs: bool) -> Vec<PathBuf> {
    let state = &locations.state.path;
    let mut paths = vec![
        Provisioning::environment_dir(state, app, id),
        Provisioning::pycache_dir(state, app, id),
        locations.run.path.join("apps").join(app).join(id),
    ];

    if !keep_logs {
        paths.extend(release_logs(locations, app, id));
    }

    paths
}

/// Measure what a release's files take, for what removing it gives back.
pub fn measure_release(locations: &Locations, app: &str, id: &str, keep_logs: bool) -> Measure {
    let mut measure = Measure::default();

    for path in release_paths(locations, app, id, keep_logs) {
        measure.add(&path);
    }

    measure
}

/// What each of several releases gives back and what removing all of them
/// gives back together, for the question asked before a removal.
pub fn measure_releases(
    locations: &Locations,
    releases: &[(String, String)],
    keep_logs: bool,
) -> (Vec<DiskUse>, DiskUse) {
    let mut each = Vec::with_capacity(releases.len());
    let mut together = Measure::default();

    for (app, id) in releases {
        let measure = measure_release(locations, app, id, keep_logs);
        each.push(measure.total());
        together.merge(&measure);
    }

    (each, together.total())
}

/// Remove a release's files, leaving any `APP` directory that becomes
/// empty out of the way. Returns what could not be removed, each with why.
pub fn remove_files(locations: &Locations, app: &str, id: &str, keep_logs: bool) -> Vec<String> {
    let mut failures = Vec::new();
    let mut parents = Vec::new();

    for path in release_paths(locations, app, id, keep_logs) {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };

        let removed = if metadata.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };

        match removed {
            Ok(()) => parents.extend(path.parent().map(Path::to_path_buf)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }

    for parent in parents {
        let _ = std::fs::remove_dir(parent);
    }

    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReleaseState::{Failed, Ready, Retained, Stopped};

    fn release(id: &str, state: ReleaseState, created_unix_ms: u64) -> Candidate {
        Candidate {
            id: id.to_owned(),
            state,
            created_unix_ms,
            declared: false,
            referenced: false,
        }
    }

    fn removed(decision: &Decision) -> Vec<&str> {
        decision.removed.iter().map(|(id, _)| id.as_str()).collect()
    }

    fn why_kept(decision: &Decision, id: &str) -> Keep {
        decision
            .kept
            .iter()
            .find(|(kept, _)| kept == id)
            .map(|(_, why)| *why)
            .unwrap_or_else(|| panic!("{id} was not kept: {decision:?}"))
    }

    #[test]
    fn keep_zero_still_leaves_the_serving_and_rollback_releases() {
        let candidates = [
            release("r1", Stopped, 1),
            release("r2", Stopped, 2),
            release("r3", Stopped, 3),
            release("r4", Ready, 4),
        ];

        let decision = decide_prune(Some("r4"), Some("r3"), &candidates, 0, false);
        assert_eq!(removed(&decision), ["r1", "r2"]);
        assert_eq!(why_kept(&decision, "r4"), Keep::Serving);
        assert_eq!(why_kept(&decision, "r3"), Keep::Rollback);
    }

    #[test]
    fn keep_counts_the_newest_that_have_not_failed_beside_the_protected() {
        let candidates = [
            release("r1", Stopped, 1),
            release("r2", Stopped, 2),
            release("r3", Stopped, 3),
            release("r4", Stopped, 4),
            release("r5", Failed, 5),
            release("r6", Stopped, 6),
            release("r7", Ready, 7),
        ];

        let decision = decide_prune(Some("r7"), Some("r6"), &candidates, 2, false);

        // r3 and r4 are the two kept; r5 failed, and is not counted.

        assert_eq!(removed(&decision), ["r1", "r2", "r5"]);
        assert_eq!(why_kept(&decision, "r3"), Keep::WithinKeep);
        assert_eq!(why_kept(&decision, "r4"), Keep::WithinKeep);
        assert_eq!(
            decision.removed.last(),
            Some(&("r5".to_owned(), Reason::Failed))
        );
    }

    #[test]
    fn a_run_of_failed_deploys_does_not_push_out_good_releases() {
        let candidates = [
            release("r1", Stopped, 1),
            release("r2", Stopped, 2),
            release("r3", Ready, 3),
            release("r4", Failed, 4),
            release("r5", Failed, 5),
        ];

        let decision = decide_prune(Some("r3"), Some("r2"), &candidates, 1, false);
        assert!(removed(&decision).is_empty(), "{decision:?}");
        assert_eq!(why_kept(&decision, "r1"), Keep::WithinKeep);
        assert_eq!(why_kept(&decision, "r4"), Keep::FailedRecent);
        assert_eq!(why_kept(&decision, "r5"), Keep::FailedRecent);
    }

    #[test]
    fn a_failed_release_stays_until_both_serving_and_rollback_are_newer() {
        // Failed between the rollback release and the serving one.

        let candidates = [
            release("r1", Stopped, 1),
            release("r2", Failed, 2),
            release("r3", Ready, 3),
        ];
        let decision = decide_prune(Some("r3"), Some("r1"), &candidates, 0, false);
        assert!(removed(&decision).is_empty(), "{decision:?}");
        assert_eq!(why_kept(&decision, "r2"), Keep::FailedRecent);

        // Older than both, it goes whatever keep is.

        let candidates = [
            release("r1", Failed, 1),
            release("r2", Stopped, 2),
            release("r3", Ready, 3),
        ];
        let decision = decide_prune(Some("r3"), Some("r2"), &candidates, 9, false);
        assert_eq!(removed(&decision), ["r1"]);

        // A rollback swaps them: the cutoff is the older of the two, here
        // the serving release.

        let candidates = [
            release("r1", Failed, 1),
            release("r2", Ready, 2),
            release("r3", Failed, 3),
            release("r4", Stopped, 4),
        ];
        let decision = decide_prune(Some("r2"), Some("r4"), &candidates, 0, false);
        assert_eq!(removed(&decision), ["r1"]);
        assert_eq!(why_kept(&decision, "r3"), Keep::FailedRecent);
    }

    #[test]
    fn an_application_without_both_keeps_its_failed_releases() {
        let candidates = [release("r1", Failed, 1), release("r2", Failed, 2)];
        let decision = decide_prune(None, None, &candidates, 0, false);
        assert!(removed(&decision).is_empty(), "{decision:?}");

        let candidates = [release("r1", Failed, 1), release("r2", Ready, 2)];
        let decision = decide_prune(Some("r2"), None, &candidates, 0, false);
        assert!(removed(&decision).is_empty(), "{decision:?}");
        assert_eq!(why_kept(&decision, "r1"), Keep::FailedRecent);
    }

    #[test]
    fn running_retained_and_referenced_releases_are_never_taken() {
        let mut referenced = release("r2", Stopped, 2);
        referenced.referenced = true;
        let candidates = [
            release("r1", Retained, 1),
            referenced,
            release("r3", ReleaseState::Pending, 3),
            release("r4", Ready, 4),
        ];

        let decision = decide_prune(Some("r4"), None, &candidates, 0, true);
        assert!(removed(&decision).is_empty(), "{decision:?}");
        assert_eq!(why_kept(&decision, "r1"), Keep::InUse(Retained));
        assert_eq!(why_kept(&decision, "r2"), Keep::Referenced);
        assert_eq!(
            why_kept(&decision, "r3"),
            Keep::InUse(ReleaseState::Pending)
        );
    }

    #[test]
    fn a_declared_release_stays_unless_forced() {
        let mut declared = release("r1", Stopped, 1);
        declared.declared = true;
        let mut failed = release("r2", Failed, 2);
        failed.declared = true;
        let candidates = [
            declared,
            failed,
            release("r3", Stopped, 3),
            release("r4", Stopped, 4),
            release("r5", Ready, 5),
        ];

        let decision = decide_prune(Some("r5"), Some("r4"), &candidates, 0, false);
        assert_eq!(removed(&decision), ["r3"]);
        assert_eq!(why_kept(&decision, "r1"), Keep::Declared);
        assert_eq!(why_kept(&decision, "r2"), Keep::Declared);

        let decision = decide_prune(Some("r5"), Some("r4"), &candidates, 0, true);
        assert_eq!(removed(&decision), ["r1", "r2", "r3"]);
    }

    #[test]
    fn the_files_of_a_release_go_and_its_source_is_not_among_them() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let locations = Locations::resolve(&crate::locations::Options {
            state_dir: Some(root.join("state")),
            run_dir: Some(root.join("run")),
            log_dir: Some(root.join("logs")),
            config: Some(root.join("muru.toml")),
            ..Default::default()
        });
        let write = |path: PathBuf| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"content").unwrap();
        };

        write(root.join("state/envs/shop/r1/lib/module.py"));
        write(root.join("state/pycache/shop/r1/module.pyc"));
        write(root.join("run/apps/shop/r1/host.toml"));
        write(root.join("logs/apps/shop/r1.log"));
        write(root.join("logs/apps/shop/r1.log.20260101-000000.zst"));
        write(root.join("logs/apps/shop/r12.log"));
        write(root.join("state/envs/other/r1/lib/module.py"));

        // Keeping the logs takes everything else.

        let failures = remove_files(&locations, "shop", "r1", true);
        assert!(failures.is_empty(), "{failures:?}");
        assert!(!root.join("state/envs/shop").exists(), "empty parent goes");
        assert!(!root.join("state/pycache/shop").exists());
        assert!(!root.join("run/apps/shop/r1").exists());
        assert!(root.join("logs/apps/shop/r1.log").exists());
        assert!(root.join("state/envs/other/r1/lib/module.py").exists());

        // Without --keep-logs the log and archives go, and r12's log stays.

        let failures = remove_files(&locations, "shop", "r1", false);
        assert!(failures.is_empty(), "{failures:?}");
        assert!(!root.join("logs/apps/shop/r1.log").exists());
        assert!(
            !root
                .join("logs/apps/shop/r1.log.20260101-000000.zst")
                .exists()
        );
        assert!(root.join("logs/apps/shop/r12.log").exists());
    }
}
