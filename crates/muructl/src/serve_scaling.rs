//! Scaling under `muructl serve` (design specification section 13.9,
//! issue 623): each group's configured policy, driven each second by the
//! same per-group step as the daemon's, with a scale-out admitted against
//! the container's memory, as the daemon's budget admits one against the
//! node's, and the memory guard scaling a group with a policy in rather
//! than replacing its worker.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use muru_config::format_size_approx as size;
use muru_controller::budget::{Budget, Cost, Usage};
use muru_controller::cgroup::Limits;
use muru_controller::host::{HostSpec, WorkerStatus};
use muru_controller::model::ScalingSpec;
use muru_controller::scaling::{GroupScaling, Step, WorkerSignals};

/// The scaling shared by the run's loop, its sampling and its admin
/// socket.
pub type Shared = Arc<Mutex<Scaling>>;

/// An event to raise: its kind, level and message.
pub type Event = (&'static str, &'static str, String);

/// One release's groups' scaling.
pub struct Scaling {
    app: String,
    release: String,
    groups: Vec<GroupScaling>,
    sizes: HashMap<String, (u32, u32, u32)>,
    interface: muru_config::Interface,
    hard_limits: HashMap<String, u64>,
    limits: Option<Limits>,
    signals_from: Option<(Instant, Vec<WorkerStatus>)>,
}

impl Scaling {
    /// The scaling of `spec`'s groups by their policies, the default
    /// group's first, as `app@release`, with the container's `limits` to
    /// admit scale-outs against.
    pub fn new(
        app: &str,
        release: &str,
        spec: &HostSpec,
        policies: (ScalingSpec, Vec<(String, ScalingSpec)>),
        interface: muru_config::Interface,
        limits: Option<Limits>,
    ) -> Self {
        let (default, named) = policies;
        let soft = |memory: &muru_controller::model::MemoryPolicy| {
            memory.soft_limit.map(|limit| (memory.measure, limit))
        };
        let mut groups = vec![GroupScaling::new(
            muru_edge::routing::DEFAULT_GROUP,
            default,
            spec.processes as u32,
            soft(&spec.memory),
        )];
        let mut hard_limits = HashMap::new();

        if let Some(limit) = spec.memory.hard_limit {
            hard_limits.insert(muru_edge::routing::DEFAULT_GROUP.to_owned(), limit);
        }

        (groups[0].slots_per_worker, groups[0].threads) = muru_controller::scaling::worker_size(
            interface,
            spec.threads as u32,
            spec.sync_threads as u32,
            spec.concurrency as u32,
        );

        for group in &spec.groups {
            let policy = named
                .iter()
                .find(|(name, _)| *name == group.name)
                .map(|(_, policy)| policy.clone())
                .unwrap_or_default();
            let mut scaling = GroupScaling::new(
                &group.name,
                policy,
                group.processes as u32,
                soft(&group.memory),
            );
            (scaling.slots_per_worker, scaling.threads) = muru_controller::scaling::worker_size(
                interface,
                group.threads as u32,
                group.sync_threads as u32,
                group.concurrency as u32,
            );

            if let Some(limit) = group.memory.hard_limit {
                hard_limits.insert(group.name.clone(), limit);
            }

            groups.push(scaling);
        }

        let mut sizes = HashMap::new();
        sizes.insert(
            muru_edge::routing::DEFAULT_GROUP.to_owned(),
            (
                spec.threads as u32,
                spec.sync_threads as u32,
                spec.concurrency as u32,
            ),
        );

        for group in &spec.groups {
            sizes.insert(
                group.name.clone(),
                (
                    group.threads as u32,
                    group.sync_threads as u32,
                    group.concurrency as u32,
                ),
            );
        }

        Self {
            app: app.to_owned(),
            release: release.to_owned(),
            groups,
            sizes,
            interface,
            hard_limits,
            limits,
            signals_from: None,
        }
    }

    /// Size the groups for the interface the host settled, which it
    /// announces once it has loaded the application, as the daemon does.
    pub fn sized_for(&mut self, interface: muru_config::Interface) {
        if interface == self.interface {
            return;
        }

        self.interface = interface;

        for group in &mut self.groups {
            if let Some((threads, sync_threads, concurrency)) = self.sizes.get(&group.name) {
                (group.slots_per_worker, group.threads) = muru_controller::scaling::worker_size(
                    interface,
                    *threads,
                    *sync_threads,
                    *concurrency,
                );
            }
        }
    }

    /// Each group's most workers: its `max_workers` under a policy, else
    /// its count, for the start's warning of more workers than CPUs.
    pub fn most_workers(&self) -> HashMap<String, u32> {
        self.groups
            .iter()
            .map(|group| (group.name.clone(), group.scaler.spec().max_workers))
            .collect()
    }

    /// Each group's fewest workers under a policy, for `--min-ready`.
    pub fn fewest_workers(&self) -> Vec<(String, u32)> {
        self.groups
            .iter()
            .filter(|group| !group.scaler.spec().is_static())
            .map(|group| (group.name.clone(), group.scaler.spec().min_workers))
            .collect()
    }

    /// The workers' own signals, from what grew since reports a few
    /// seconds older (section 7.3), as the daemon takes them.
    pub fn statuses(&mut self, workers: &[WorkerStatus]) {
        let now = Instant::now();

        let Some((at, before)) = &self.signals_from else {
            self.signals_from = Some((now, workers.to_vec()));
            return;
        };

        let elapsed = now.saturating_duration_since(*at);

        if elapsed < std::time::Duration::from_secs(4) {
            return;
        }

        for group in &mut self.groups {
            let signals =
                WorkerSignals::between(before, workers, &group.name, elapsed, group.threads);
            group.scaler.set_workers(signals, false);
        }

        self.signals_from = Some((now, workers.to_vec()));
    }

    /// One second's step: each group observed on the edge's samples with
    /// `ready` workers routed to in each, a scale-out admitted against
    /// the container's memory, none while the guard acts. Returns the
    /// counts to ask of the host and the events to raise.
    pub fn step(
        &mut self,
        samples: &[muru_edge::metrics::Sample],
        ready: &HashMap<String, u32>,
        workers: &[WorkerStatus],
        guard_acting: bool,
    ) -> (Vec<(String, u32)>, Vec<Event>) {
        let now = Instant::now();
        let mut events = Vec::new();
        let mut requests = Vec::new();
        let memory = self
            .limits
            .as_ref()
            .and_then(|limits| limits.again().memory());

        for index in 0..self.groups.len() {
            let name = self.groups[index].name.clone();
            let ready_now = ready.get(&name).copied().unwrap_or(0);
            let cost = self.cost(&name, workers);
            let group = &mut self.groups[index];

            match group.observe(
                now,
                samples,
                &self.app,
                &self.release,
                ready_now,
                None,
                None,
            ) {
                Step::Ask { from, to, reason } => {
                    let (admitted, held) = admit(memory, guard_acting, from, to, cost);
                    let limit = (admitted < to).then_some(muru_controller::budget::Limit::Memory);

                    events.extend(group.admitted(
                        now, ready_now, from, to, admitted, limit, &held, &reason, None,
                    ));
                }
                Step::Event(kind, level, message) => events.push((kind, level, message)),
                Step::Nothing => {}
            }

            if let Some(desired) = self.groups[index].wanted(ready_now) {
                requests.push((name, desired));
            }
        }

        (requests, events)
    }

    /// Sections 7.7 and 13.9: the memory guard would replace a worker of
    /// `group`, which has `current` workers. A group with a policy above
    /// its `min_workers` is scaled in by that worker instead: true, with
    /// the event, when it is.
    pub fn scale_in_for_memory(
        &mut self,
        group: &str,
        current: u32,
        reason: &str,
    ) -> Option<Event> {
        let scaling = self
            .groups
            .iter_mut()
            .find(|scaling| scaling.name == group)?;
        let spec = scaling.scaler.spec();

        if spec.is_static() || spec.advisory || current <= spec.min_workers {
            return None;
        }

        if scaling.scaler.pinned().is_some() {
            return None;
        }

        let to = current - 1;
        let why = format!("for memory: {reason}");
        scaling.scaler.shrink(current, to, Instant::now(), &why);
        scaling.requested = Some(to);

        Some((
            "scaling.decision",
            "info",
            muru_controller::scaling::decision_message(group, current, to, &why, false),
        ))
    }

    /// Each group's state, for `muructl scale status`.
    pub fn status(&self) -> muru_proto::admin::v1::ScaleStatus {
        let now = Instant::now();

        muru_controller::api::scale_message(
            &self.app,
            &self.release,
            self.groups
                .iter()
                .map(|group| (group.name.clone(), group.scaler.status(now)))
                .collect(),
        )
    }

    /// What one more worker of `group` counts as: its `hard_limit` when
    /// one is set, else what its workers measure, else its soft limit,
    /// else the mean of every worker, as the daemon's budget costs one.
    fn cost(&self, group: &str, workers: &[WorkerStatus]) -> Cost {
        let mean = |sizes: Vec<u64>| {
            (!sizes.is_empty()).then(|| sizes.iter().sum::<u64>() / sizes.len() as u64)
        };
        let measured = mean(
            workers
                .iter()
                .filter(|worker| worker.group == group)
                .filter_map(|worker| worker.pss)
                .collect(),
        );
        let soft = self
            .groups
            .iter()
            .find(|scaling| scaling.name == group)
            .and_then(|scaling| scaling.memory_limit.map(|(_, limit)| limit));
        let everyone = mean(workers.iter().filter_map(|worker| worker.pss).collect());

        Cost {
            memory: self
                .hard_limits
                .get(group)
                .copied()
                .or(measured)
                .or(soft)
                .or(everyone),
            cpu: None,
        }
    }
}

/// How many of the workers a scale-out from `from` to `to` asks for fit
/// under the container's memory, with what held it back: the budget is
/// `memory.max` less the headroom, and what is used is what the cgroup
/// uses now. While the guard acts nothing fits.
fn admit(
    memory: Option<muru_controller::cgroup::Memory>,
    guard_acting: bool,
    from: u32,
    to: u32,
    cost: Cost,
) -> (u32, String) {
    let Some(memory) = memory else {
        return (to, String::new());
    };

    if guard_acting {
        return (
            from,
            "memory.max, while the memory guard is acting".to_owned(),
        );
    }

    let budget = Budget {
        memory: Some(memory.limit.saturating_sub(memory.headroom())),
        ..Budget::default()
    };
    let used = Usage {
        memory: memory.used,
        ..Usage::default()
    };
    let ask = muru_controller::budget::Ask {
        key: (),
        priority: 0,
        from,
        to,
        cost,
    };
    let admitted = muru_controller::budget::admit(&budget, used, vec![ask])
        .pop()
        .map_or(from, |admitted| admitted.to);
    let room = memory.left().saturating_sub(memory.headroom());
    let held = format!(
        "memory.max: a worker counts {}, and {} is left above the headroom of {}",
        cost.memory.map_or_else(|| "nothing known".to_owned(), size),
        size(room),
        size(memory.headroom())
    );

    (admitted, held)
}

#[cfg(test)]
mod tests {
    use super::*;
    use muru_controller::cgroup::Memory;

    const MB: u64 = 1 << 20;

    fn spare(min: u32, max: u32) -> ScalingSpec {
        ScalingSpec {
            mode: muru_config::ScalingMode::Spare,
            min_workers: min,
            max_workers: max,
            ..ScalingSpec::default()
        }
    }

    fn scaling(hard_limit: Option<u64>) -> Scaling {
        Scaling {
            app: "shop".to_owned(),
            release: "r1".to_owned(),
            groups: vec![GroupScaling::new("default", spare(1, 4), 2, None)],
            sizes: HashMap::new(),
            interface: muru_config::Interface::Wsgi,
            hard_limits: hard_limit
                .map(|limit| ("default".to_owned(), limit))
                .into_iter()
                .collect(),
            limits: None,
            signals_from: None,
        }
    }

    fn worker(index: u32, pss: u64) -> WorkerStatus {
        WorkerStatus {
            index,
            pid: 100 + index,
            group: "default".to_owned(),
            pss: Some(pss),
            ..WorkerStatus::default()
        }
    }

    #[test]
    fn a_worker_counts_at_its_hard_limit_when_one_is_set_else_at_what_it_measures() {
        let workers = [worker(0, 100 * MB), worker(1, 140 * MB)];

        assert_eq!(
            scaling(Some(300 * MB)).cost("default", &workers).memory,
            Some(300 * MB)
        );
        assert_eq!(
            scaling(None).cost("default", &workers).memory,
            Some(120 * MB)
        );
        assert_eq!(scaling(None).cost("default", &[]).memory, None);
    }

    #[test]
    fn a_scale_out_is_admitted_for_the_workers_that_fit_above_the_headroom() {
        // 1GB with 128MB of headroom: 896MB to use, 500MB used, so 396MB.

        let memory = Some(Memory {
            limit: 1 << 30,
            used: 500 * MB,
        });
        let at = |each: u64| Cost {
            memory: Some(each * MB),
            cpu: None,
        };

        assert_eq!(admit(memory, false, 2, 4, at(150)).0, 4);
        assert_eq!(admit(memory, false, 2, 4, at(300)).0, 3);

        let (admitted, held) = admit(memory, false, 2, 4, at(400));
        assert_eq!(admitted, 2);
        assert_eq!(
            held,
            "memory.max: a worker counts 400MB, and 396MB is left above the headroom of 128MB"
        );

        // Nothing while the guard acts, everything without a limit.

        assert_eq!(admit(memory, true, 2, 4, at(1)).0, 2);
        assert_eq!(admit(None, false, 2, 4, at(400)).0, 4);
    }

    #[test]
    fn short_of_memory_a_group_above_its_fewest_is_scaled_in_rather_than_replaced() {
        let mut scaling = scaling(None);

        let event = scaling
            .scale_in_for_memory("default", 3, "the container was short of memory")
            .expect("a scale-in");
        assert_eq!(event.0, "scaling.decision");
        assert!(
            event
                .2
                .starts_with("default: scale in from 3 to 2 workers: for memory"),
            "{}",
            event.2
        );
        assert_eq!(scaling.groups[0].scaler.desired(), 2);
        assert_eq!(scaling.groups[0].wanted(3), None);

        // At min_workers, or without a policy, the worker is replaced.

        assert_eq!(scaling.scale_in_for_memory("default", 1, "short"), None);
        scaling.groups[0] = GroupScaling::new("default", ScalingSpec::default(), 3, None);
        assert_eq!(scaling.scale_in_for_memory("default", 3, "short"), None);
    }
}
