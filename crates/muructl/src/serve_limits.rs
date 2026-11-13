//! `muructl serve`'s container limits while it runs (design
//! specification sections 7.7 and 13.9, ADR 0061): read again at every
//! memory check, a change logged and the start warnings given again
//! against it, a spool quota taken from the headroom set again, the
//! memory guard driven from the workers' reports, and what `muructl
//! status` and `/status` show kept up to date.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use muru_controller::cgroup::Limits;
use muru_controller::guard::{self, Action, Asked, Guard};
use muru_controller::host::{HostSpec, WorkerStatus};
use muru_proto::admin::v1 as pb;

/// What `muructl status` and `/status` show of the limits, shared with
/// the admin services.
pub type SharedReport = Arc<Mutex<Option<pb::CgroupLimits>>>;

/// How a request body spool's quota follows the limits: in memory under
/// a memory limit it is the headroom unless one was set.
pub struct Spool {
    /// The spool the edge writes to.
    pub spool: Arc<muru_edge::uploads::Spool>,
    /// What serve's settings make of its quota under given limits.
    pub rule: crate::serve::SpoolRule,
}

/// The container's limits as one run watches them.
pub struct Watch {
    limits: Limits,
    guard: Guard,
    asked: Asked,
    interval: Duration,
    measures: HashMap<String, muru_config::Measure>,
    default_measure: muru_config::Measure,
    owner: String,
    host_rss: Option<u64>,
    spool: Option<Spool>,
    report: SharedReport,
}

impl Watch {
    /// Watch `limits` for the release `spec` describes, as `owner`,
    /// `app@release`, keeping what status shows in `report`.
    pub fn new(
        limits: Limits,
        spec: &HostSpec,
        owner: String,
        spool: Option<Spool>,
        report: SharedReport,
    ) -> Self {
        let mut watch = Self {
            limits,
            guard: Guard::default(),
            asked: asked(spec),
            interval: Duration::from_millis(spec.memory.check_interval_ms.max(250)),
            measures: spec
                .groups
                .iter()
                .map(|group| (group.name.clone(), group.memory.measure))
                .collect(),
            default_measure: spec.memory.measure,
            owner,
            host_rss: None,
            spool,
            report,
        };
        watch.refresh(&[]);
        watch
    }

    /// How often the limits are read: the release's memory check.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// The warnings the configuration gets now, for the start.
    pub fn warnings(&self) -> Vec<String> {
        guard::warnings(&self.limits, &self.asked)
    }

    /// The host has said what it holds. The first time, once the
    /// application is imported, that is said, with the warning a preload
    /// that would not fit when copied gets.
    pub fn host(&mut self, rss: Option<u64>) {
        let Some(rss) = rss.filter(|rss| *rss > 0) else {
            return;
        };

        let first = self.host_rss.is_none();
        self.host_rss = Some(rss);

        if !first {
            return;
        }

        muru_common::muru_println!(
            "the host holds {} once the application is imported",
            muru_config::format_size_approx(rss)
        );

        let asked = Asked {
            host: Some(rss),
            hard_limits: None,
            ..self.asked.clone()
        };

        for warning in guard::warnings(&self.limits, &asked)
            .into_iter()
            .filter(|warning| warning.starts_with("the host holds"))
        {
            muru_common::muru_println!("warning: {warning}");
        }

        self.asked.host = Some(rss);
    }

    /// One memory check: the limits read again, a change said, and the
    /// guard's decision on the workers' last reports.
    pub fn check(&mut self, workers: &[WorkerStatus]) -> Action {
        let now = self.limits.again();
        let changes = now.changes(&self.limits);

        if !changes.is_empty() {
            muru_common::muru_println!("container limits changed: {}", changes.join(", "));
            self.limits = now;

            for warning in guard::warnings(&self.limits, &self.asked) {
                muru_common::muru_println!("warning: {warning}");
            }

            if let Some(spool) = &self.spool {
                let (quota, note) = spool.rule.spool_quota_under(Some(&self.limits));
                spool.spool.set_quota(Some(quota));

                if let Some(note) = note {
                    muru_common::muru_println!("{note}");
                }
            }
        }

        self.refresh(workers)
    }

    /// Read what is used, run the guard when there is a memory limit,
    /// and update the report.
    fn refresh(&mut self, workers: &[WorkerStatus]) -> Action {
        let memory = self.limits.memory();
        let workers = self.workers(workers);
        let hosts = self.host_rss.unwrap_or(0);

        let action = match memory {
            Some(memory) => self.guard.check(memory, &workers, hosts),
            None => Action::Nothing,
        };

        *self.report.lock().expect("limits report poisoned") = Some(guard::report(
            &self.limits,
            memory,
            workers.iter().map(|worker| worker.size).sum(),
            hosts,
            Some(&self.guard),
        ));

        action
    }

    /// The workers as the guard sees them, each measured by its group's
    /// policy.
    fn workers(&self, workers: &[WorkerStatus]) -> Vec<guard::Worker> {
        workers
            .iter()
            .map(|worker| {
                let measure = self
                    .measures
                    .get(&worker.group)
                    .copied()
                    .unwrap_or(self.default_measure);

                let (size, measure) = match measure {
                    muru_config::Measure::Uss => (worker.uss, "uss"),
                    muru_config::Measure::Pss => (worker.pss, "pss"),
                    muru_config::Measure::Rss => (worker.rss, "rss"),
                };

                let name = muru_common::worker::WorkerName::new(&worker.group, worker.index);

                guard::Worker {
                    owner: self.owner.clone(),
                    group: name.group.clone(),
                    index: worker.index,
                    name: name.to_string(),
                    pid: worker.pid,
                    size: size.unwrap_or(0),
                    measure,
                }
            })
            .collect()
    }
}

/// What the release asks of the limits: its workers over every group,
/// and the hard limits times them.
fn asked(spec: &HostSpec) -> Asked {
    let groups = std::iter::once((spec.processes, spec.memory.hard_limit)).chain(
        spec.groups
            .iter()
            .map(|group| (group.processes, group.memory.hard_limit)),
    );

    let mut asked = Asked {
        groups: 1 + spec.groups.len(),
        ..Asked::default()
    };

    for (processes, hard_limit) in groups {
        asked.workers += processes;

        if let Some(limit) = hard_limit {
            let (total, count) = asked.hard_limits.unwrap_or((0, 0));
            asked.hard_limits = Some((total + limit * processes as u64, count + 1));
        }
    }

    asked
}
