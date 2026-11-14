//! The memory guard (design specification sections 7.7 and 13.9, ADR
//! 0061): wherever the workers run in a cgroup with a `memory.max`, the
//! controller reads what is left under it at every memory check and,
//! with less than the headroom left, has the largest worker replaced,
//! one at a time. It stops, with a warning, while replacing workers
//! cannot bring the cgroup back, and warns again only once the cgroup
//! has been above the headroom.
//!
//! The decision is here, apart from where the readings come from, so
//! `muructl serve` and the daemon make it the same way: each passes its
//! cgroup's reading, the workers it supervises with their size by their
//! own policy's measure, and what its hosts hold.
//!
//! Also here, the warnings a configuration gets when it does not fit the
//! limits, given at start and again when a check finds them changed.

use std::time::{Duration, Instant};

use muru_config::format_size_approx as size;

use crate::cgroup::{GUIDE, Limits, Memory};

/// How long a replacement may take before the guard stops waiting for
/// it: one that never becomes ready, as after repeated start failures,
/// must not hold the guard for good.
const REPLACEMENT_WAIT: Duration = Duration::from_secs(120);

/// One worker as the guard sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worker {
    /// Whose worker it is: the release, as `app@release`.
    pub owner: String,
    /// Its group.
    pub group: String,
    /// Its index within the group.
    pub index: u32,
    /// Its name, as events give it: the index, or `group/index`.
    pub name: String,
    /// Its pid.
    pub pid: u32,
    /// Its own memory by its policy's measure, 0 before it has reported.
    pub size: u64,
    /// The measure `size` is in: `uss`, `pss` or `rss`.
    pub measure: &'static str,
    /// Whether its host is replacing it for memory, by its own limits or
    /// the guard's ask, and its successor has not yet taken its place.
    /// Such a replacement is the guard's one at a time (issue 662), and
    /// the worker is otherwise left out of the guard's reckoning.
    pub replacing: bool,
}

/// What the guard asks for after a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do.
    Nothing,
    /// Replace this worker, with this reason, which goes in its
    /// `worker.recycled` event.
    Replace {
        /// The worker's release, `app@release`.
        owner: String,
        /// The worker's group.
        group: String,
        /// The worker's index within its group.
        index: u32,
        /// Its pid, so that a worker replaced since is left alone.
        pid: u32,
        /// Why, for the event.
        reason: String,
    },
    /// Replacing workers would not help: log this warning.
    Hold(String),
}

/// The guard's state, as `muructl status` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Nothing is being done.
    Watching,
    /// A worker is being replaced; the next waits until it has been.
    Replacing {
        owner: String,
        group: String,
        index: u32,
        name: String,
        pid: u32,
        left_before: u64,
        since: Instant,
        /// Whether a successor takes its place: not when its group is
        /// scaled in by it instead (issue 623).
        successor: bool,
    },
    /// A replacement left no more room than there was, so none is
    /// replaced until the cgroup has been above the headroom again.
    Holding(String),
    /// The workers' own memory cannot make up the shortfall as they last
    /// reported it. Judged again at every check, since reports trail the
    /// cgroup's reading, but warned of once until the cgroup has been
    /// above the headroom.
    Waiting(String),
}

/// The last worker the guard replaced, for status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    /// `app@release worker NAME (pid N)`.
    pub worker: String,
    /// When, in Unix milliseconds.
    pub at_unix_ms: u64,
}

/// The memory guard of one cgroup.
#[derive(Debug, Clone)]
pub struct Guard {
    state: State,
    last: Option<Replaced>,
    /// Whether the cgroup was already short, with too little worker
    /// memory to make it up, at the check before: the workers' reports
    /// may not yet show what took the room, so that is warned of only
    /// when a second check finds the same.
    short_before: bool,
}

impl Default for Guard {
    fn default() -> Self {
        Self {
            state: State::Watching,
            last: None,
            short_before: false,
        }
    }
}

impl Guard {
    /// One check: `memory` is the cgroup's reading now, `workers` every
    /// worker in it, `hosts` what the hosts hold, added up.
    pub fn check(&mut self, memory: Memory, workers: &[Worker], hosts: u64) -> Action {
        self.check_at(memory, workers, hosts, Instant::now())
    }

    fn check_at(&mut self, memory: Memory, all: &[Worker], hosts: u64, now: Instant) -> Action {
        let left = memory.left();
        let headroom = memory.headroom();
        let (replacing, workers): (Vec<Worker>, Vec<Worker>) =
            all.iter().cloned().partition(|worker| worker.replacing);
        let workers = &workers[..];

        // A replacement in hand is waited for: the next worker is chosen
        // once the old one has gone and its successor has reported. One
        // that left no more room than there was shows that replacing
        // workers does not help.

        if let State::Replacing {
            owner,
            group,
            index,
            name,
            pid,
            left_before,
            since,
            successor,
        } = &self.state
        {
            let gone = !workers.iter().any(|worker| worker.pid == *pid);
            let succeeded = !successor
                || workers.iter().any(|worker| {
                    worker.owner == *owner
                        && worker.group == *group
                        && worker.index == *index
                        && worker.size > 0
                });

            // The worker chosen still serves while another is replaced for
            // memory: its host refused, since that replacement is the one
            // at a time (issue 662), and the guard judges again once it
            // has finished.

            if !gone && replacing.iter().any(|worker| worker.pid != *pid) {
                self.state = State::Watching;
                return Action::Nothing;
            }

            if !(gone && succeeded) && now.duration_since(*since) < REPLACEMENT_WAIT {
                return Action::Nothing;
            }

            if left < headroom && left <= *left_before {
                let warning = format!(
                    "the container is still short of memory after worker {name} of {owner} was \
                     replaced: {} left under memory.max {}, less than the headroom of {}, no \
                     more than before; {}",
                    size(left),
                    size(memory.limit),
                    size(headroom),
                    held(memory, workers, hosts)
                );
                self.state = State::Holding(warning.clone());
                return Action::Hold(warning);
            }

            self.state = State::Watching;
        }

        if left >= headroom {
            self.state = State::Watching;
            self.short_before = false;
            return Action::Nothing;
        }

        if matches!(self.state, State::Holding(_)) {
            return Action::Nothing;
        }

        // A worker being replaced for memory, by its own limits or an
        // earlier ask, is the one at a time (issue 662): the next is
        // chosen once its successor has taken its place.

        if !replacing.is_empty() {
            return Action::Nothing;
        }

        // Until a worker has reported its memory, as just after a start,
        // there is nothing to judge by.

        if !workers.is_empty() && workers.iter().all(|worker| worker.size == 0) {
            return Action::Nothing;
        }

        // Replacing workers can free at most their own memory.

        let shortfall = headroom - left;
        let own: u64 = workers.iter().map(|worker| worker.size).sum();
        let largest = workers
            .iter()
            .filter(|worker| worker.size > 0)
            .max_by_key(|worker| worker.size);

        let Some(largest) = largest.filter(|_| own >= shortfall) else {
            if matches!(self.state, State::Waiting(_)) {
                return Action::Nothing;
            }

            if !self.short_before {
                self.short_before = true;
                return Action::Nothing;
            }

            let warning = format!(
                "the container is short of memory, {} left under memory.max {}, less than the \
                 headroom of {}, and replacing workers would not help: {}",
                size(left),
                size(memory.limit),
                size(headroom),
                held(memory, workers, hosts)
            );
            self.state = State::Waiting(warning.clone());
            return Action::Hold(warning);
        };

        let reason = format!(
            "the container was short of memory, {} left under memory.max {}, less than the \
             headroom of {}; it was the largest worker, {} {}",
            size(left),
            size(memory.limit),
            size(headroom),
            largest.measure,
            size(largest.size)
        );

        self.state = State::Replacing {
            owner: largest.owner.clone(),
            group: largest.group.clone(),
            index: largest.index,
            name: largest.name.clone(),
            pid: largest.pid,
            left_before: left,
            since: now,
            successor: true,
        };
        self.last = Some(Replaced {
            worker: format!(
                "{} worker {} (pid {})",
                largest.owner, largest.name, largest.pid
            ),
            at_unix_ms: crate::model::now_unix_ms(),
        });

        Action::Replace {
            owner: largest.owner.clone(),
            group: largest.group.clone(),
            index: largest.index,
            pid: largest.pid,
            reason,
        }
    }

    /// The state as `muructl status` names it: `watching`, `replacing`
    /// or `holding`, with what it is doing in words.
    pub fn state(&self) -> (&'static str, String) {
        match &self.state {
            State::Watching => ("watching", String::new()),
            State::Replacing {
                owner,
                name,
                pid,
                successor,
                ..
            } => (
                "replacing",
                if *successor {
                    format!("replacing {owner} worker {name} (pid {pid})")
                } else {
                    format!("scaling in by {owner} worker {name} (pid {pid})")
                },
            ),
            State::Holding(why) | State::Waiting(why) => ("holding", why.clone()),
        }
    }

    /// The worker chosen is not replaced but its group scaled in by it,
    /// so it is done once the worker has gone (issue 623).
    pub fn scaled_in(&mut self) {
        if let State::Replacing { successor, .. } = &mut self.state {
            *successor = false;
        }
    }

    /// The last worker replaced, if any has been.
    pub fn last(&self) -> Option<&Replaced> {
        self.last.as_ref()
    }

    /// Whether the guard is acting: replacing a worker or holding off.
    pub fn acting(&self) -> bool {
        !matches!(self.state, State::Watching)
    }
}

/// The limits as read now, what the workers and the hosts hold, and the
/// guard's state, as `muructl status` and `/status` give them.
pub fn report(
    limits: &Limits,
    memory: Option<Memory>,
    workers: u64,
    hosts: u64,
    guard: Option<&Guard>,
) -> muru_proto::admin::v1::CgroupLimits {
    let (state, note) = guard.map_or(("", String::new()), Guard::state);
    let last = guard.and_then(Guard::last);

    muru_proto::admin::v1::CgroupLimits {
        cgroup: limits.dir.display().to_string(),
        cpu_quota: limits
            .cpu_max
            .map_or(0.0, |(quota, period)| quota as f64 / period.max(1) as f64),
        cpus: limits.cpus.map_or(0, |cpus| cpus as u32),
        memory_max: limits.memory_max.unwrap_or(0),
        memory_used: memory.map_or(0, |memory| memory.used),
        memory_left: memory.map_or(0, |memory| memory.left()),
        headroom: limits.headroom().unwrap_or(0),
        workers_memory: workers,
        host_memory: hosts,
        guard: if limits.memory_max.is_some() {
            state.to_owned()
        } else {
            String::new()
        },
        guard_note: note,
        last_replaced: last.map(|last| last.worker.clone()).unwrap_or_default(),
        last_replaced_unix_ms: last.map_or(0, |last| last.at_unix_ms),
    }
}

/// What holds the memory, for a warning: the workers' own, the hosts',
/// and the rest of the cgroup's.
fn held(memory: Memory, workers: &[Worker], hosts: u64) -> String {
    let own: u64 = workers.iter().map(|worker| worker.size).sum();
    let outside = memory.used.saturating_sub(own).saturating_sub(hosts);

    let most = if outside >= hosts && outside >= own {
        "most of it is outside the workers and the hosts"
    } else if hosts >= own {
        "most of it is the hosts'"
    } else {
        "most of it is the workers'"
    };

    format!(
        "the workers' own memory comes to {}, the hosts hold {}, and {} is outside them \
         (muructl, the edge, the page cache and files on memory-backed mounts such as a tmpfs \
         /tmp); {most}; no worker is replaced until the container has been above the headroom \
         again; see {GUIDE}",
        size(own),
        size(hosts),
        size(outside)
    )
}

/// What a configuration asks of the limits, for the warnings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Asked {
    /// The workers of every group, a group with a scaling policy at its
    /// most.
    pub workers: usize,
    /// The groups, the default among them.
    pub groups: usize,
    /// Each group's `hard_limit` times its most workers, added up over
    /// the groups that set one, with how many groups do.
    pub hard_limits: Option<(u64, usize)>,
    /// What the host holds once the application is imported, once known.
    pub host: Option<u64>,
}

/// The warnings `asked` gets against `limits`, each with a link to the
/// guide. A warning changes nothing.
pub fn warnings(limits: &Limits, asked: &Asked) -> Vec<String> {
    let mut warnings = Vec::new();

    if let Some((cpus, from)) = limits.cpus_allowed()
        && asked.workers > cpus
    {
        let over = if asked.groups > 1 {
            format!(" over {} groups", asked.groups)
        } else {
            String::new()
        };
        let noun = if cpus == 1 { "CPU" } else { "CPUs" };

        warnings.push(format!(
            "{} workers{over} is more than the {cpus} {noun} {from} allows; see {GUIDE}",
            asked.workers
        ));
    }

    let Some(limit) = limits.memory_max else {
        return warnings;
    };

    let headroom = crate::cgroup::headroom(limit);

    // In CPython a worker's share of the host erodes as reference counts
    // are written, until it holds a copy of its own.

    if let Some(host) = asked.host.filter(|host| *host > 0) {
        let needed = host
            .saturating_mul(asked.workers as u64 + 1)
            .saturating_add(headroom);

        if needed > limit {
            let fit = (limit.saturating_sub(headroom) / host).saturating_sub(1);

            let fit = match fit {
                0 => "no worker fits".to_owned(),
                1 => "1 worker fits".to_owned(),
                count => format!("{count} workers fit"),
            };

            let workers = if asked.workers == 1 {
                "1 worker that comes".to_owned()
            } else {
                format!("{} workers that each come", asked.workers)
            };

            warnings.push(format!(
                "the host holds {}, and {workers} to copy it need {} with the headroom, more \
                 than memory.max {}; {fit}; see {GUIDE}",
                size(host),
                size(needed),
                size(limit)
            ));
        }
    }

    if let Some((total, groups)) = asked.hard_limits
        && total > limit
    {
        let whose = if groups == 1 {
            "the hard limit times the most workers comes".to_owned()
        } else {
            format!("the hard limits of {groups} groups, each times its most workers, come")
        };

        warnings.push(format!(
            "{whose} to {}, more than memory.max {}; see {GUIDE}",
            size(total),
            size(limit)
        ));
    }

    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;
    const GB: u64 = 1 << 30;

    fn worker(index: u32, pid: u32, size: u64) -> Worker {
        Worker {
            owner: "shop@r1".to_owned(),
            group: "default".to_owned(),
            index,
            name: index.to_string(),
            pid,
            size,
            measure: "uss",
            replacing: false,
        }
    }

    fn being_replaced(index: u32, pid: u32) -> Worker {
        Worker {
            replacing: true,
            ..worker(index, pid, 0)
        }
    }

    fn reading(used: u64) -> Memory {
        Memory { limit: GB, used }
    }

    #[test]
    fn with_room_to_spare_nothing_is_done() {
        let mut guard = Guard::default();
        let workers = [worker(0, 10, 300 * MB), worker(1, 11, 280 * MB)];

        assert_eq!(
            guard.check(reading(800 * MB), &workers, 250 * MB),
            Action::Nothing
        );
        assert_eq!(guard.state().0, "watching");
    }

    #[test]
    fn short_of_the_headroom_the_largest_worker_is_replaced_one_at_a_time() {
        let mut guard = Guard::default();
        let start = Instant::now();
        let workers = [worker(0, 10, 330 * MB), worker(1, 11, 280 * MB)];

        let action = guard.check_at(reading(900 * MB), &workers, 250 * MB, start);
        let Action::Replace {
            index, pid, reason, ..
        } = action
        else {
            panic!("expected a replacement, got {action:?}");
        };

        assert_eq!((index, pid), (0, 10));
        assert_eq!(
            reason,
            "the container was short of memory, 124MB left under memory.max 1.0GB, less than \
             the headroom of 128MB; it was the largest worker, uss 330MB"
        );
        assert_eq!(guard.state().0, "replacing");

        // Still short, but the replacement has not finished: no second.

        let replacing = [worker(1, 11, 300 * MB)];
        let later = start + Duration::from_secs(5);
        assert_eq!(
            guard.check_at(reading(950 * MB), &replacing, 250 * MB, later),
            Action::Nothing
        );

        // Its successor has reported: room again, back to watching.

        let replaced = [worker(0, 20, 15 * MB), worker(1, 11, 300 * MB)];
        let later = start + Duration::from_secs(10);
        assert_eq!(
            guard.check_at(reading(600 * MB), &replaced, 250 * MB, later),
            Action::Nothing
        );
        assert_eq!(guard.state().0, "watching");
        assert_eq!(
            guard.last().map(|last| last.worker.as_str()),
            Some("shop@r1 worker 0 (pid 10)")
        );
    }

    #[test]
    fn when_the_workers_cannot_make_up_the_shortfall_it_holds_and_warns_once() {
        let mut guard = Guard::default();

        // 60MB of workers cannot free the 100MB short: something else,
        // a tmpfs say, holds the memory.

        let workers = [worker(0, 10, 30 * MB), worker(1, 11, 30 * MB)];

        // The first check waits for the workers' next reports, which may
        // not yet show what took the room.

        assert_eq!(
            guard.check(reading(GB - 28 * MB), &workers, 100 * MB),
            Action::Nothing
        );
        let action = guard.check(reading(GB - 28 * MB), &workers, 100 * MB);
        let Action::Hold(warning) = action else {
            panic!("expected a hold, got {action:?}");
        };
        assert!(
            warning.contains("replacing workers would not help"),
            "{warning}"
        );
        assert!(
            warning.contains("most of it is outside the workers and the hosts"),
            "{warning}"
        );
        assert_eq!(guard.state().0, "holding");

        // Still short: no second warning, no replacement.

        assert_eq!(
            guard.check(reading(GB - 20 * MB), &workers, 100 * MB),
            Action::Nothing
        );

        // Back above the headroom, then short again: it acts again.

        assert_eq!(
            guard.check(reading(500 * MB), &workers, 100 * MB),
            Action::Nothing
        );
        assert_eq!(guard.state().0, "watching");
        assert_eq!(
            guard.check(reading(GB - 28 * MB), &workers, 100 * MB),
            Action::Nothing
        );
        assert!(matches!(
            guard.check(reading(GB - 28 * MB), &workers, 100 * MB),
            Action::Hold(_)
        ));
    }

    #[test]
    fn a_worker_whose_report_trails_the_reading_is_replaced_once_it_reports() {
        let mut guard = Guard::default();

        // The cgroup already holds the 400MB a worker took, but the
        // worker last reported 4MB: no verdict yet.

        let stale = [worker(0, 10, 4 * MB)];
        assert_eq!(
            guard.check(reading(GB - 75 * MB), &stale, 33 * MB),
            Action::Nothing
        );

        // Its next report shows it: it is replaced, with no warning.

        let fresh = [worker(0, 10, 404 * MB)];
        assert!(matches!(
            guard.check(reading(GB - 75 * MB), &fresh, 33 * MB),
            Action::Replace { index: 0, .. }
        ));

        // Had it warned, a later report that shows the room taken still
        // gets the worker replaced.

        let mut guard = Guard::default();
        assert_eq!(
            guard.check(reading(GB - 75 * MB), &stale, 33 * MB),
            Action::Nothing
        );
        assert!(matches!(
            guard.check(reading(GB - 75 * MB), &stale, 33 * MB),
            Action::Hold(_)
        ));
        assert!(matches!(
            guard.check(reading(GB - 75 * MB), &fresh, 33 * MB),
            Action::Replace { index: 0, .. }
        ));
    }

    #[test]
    fn a_replacement_that_leaves_no_more_room_stops_the_guard() {
        let mut guard = Guard::default();
        let start = Instant::now();
        let workers = [worker(0, 10, 200 * MB), worker(1, 11, 100 * MB)];

        assert!(matches!(
            guard.check_at(reading(950 * MB), &workers, 300 * MB, start),
            Action::Replace { .. }
        ));

        // The successor is ready, and the cgroup is shorter than before.

        let replaced = [worker(0, 20, 15 * MB), worker(1, 11, 100 * MB)];
        let action = guard.check_at(
            reading(960 * MB),
            &replaced,
            300 * MB,
            start + Duration::from_secs(10),
        );
        let Action::Hold(warning) = action else {
            panic!("expected a hold, got {action:?}");
        };
        assert!(
            warning.contains("still short of memory after worker 0"),
            "{warning}"
        );
    }

    #[test]
    fn a_replacement_that_never_finishes_is_not_waited_for_forever() {
        let mut guard = Guard::default();
        let start = Instant::now();
        let workers = [worker(0, 10, 330 * MB), worker(1, 11, 280 * MB)];

        assert!(matches!(
            guard.check_at(reading(900 * MB), &workers, 250 * MB, start),
            Action::Replace { .. }
        ));

        // Its successor never reports, but the old worker has gone and
        // the room came back.

        let gone = [worker(1, 11, 280 * MB)];
        let action = guard.check_at(reading(600 * MB), &gone, 250 * MB, start + REPLACEMENT_WAIT);
        assert_eq!(action, Action::Nothing);
        assert_eq!(guard.state().0, "watching");
    }

    #[test]
    fn a_worker_being_replaced_for_memory_is_the_one_at_a_time() {
        let mut guard = Guard::default();

        // Worker 1 is being replaced for its hard limit: no second.

        let workers = [worker(0, 10, 379 * MB), being_replaced(1, 11)];
        assert_eq!(
            guard.check(reading(GB - 122 * MB), &workers, 100 * MB),
            Action::Nothing
        );
        assert_eq!(guard.state().0, "watching");

        // Its successor has taken its place: the guard acts again.

        let replaced = [worker(0, 10, 379 * MB), worker(1, 20, 15 * MB)];
        assert!(matches!(
            guard.check(reading(GB - 122 * MB), &replaced, 100 * MB),
            Action::Replace { index: 0, .. }
        ));
    }

    #[test]
    fn a_choice_the_host_refused_is_dropped() {
        let mut guard = Guard::default();
        let start = Instant::now();
        let workers = [worker(0, 10, 379 * MB), worker(1, 11, 300 * MB)];

        assert!(matches!(
            guard.check_at(reading(GB - 122 * MB), &workers, 100 * MB, start),
            Action::Replace { index: 0, .. }
        ));

        // Worker 1 went over its hard limit at the same check, so the
        // host refused: worker 0 still serves.

        let refused = [worker(0, 10, 379 * MB), being_replaced(1, 11)];
        let later = start + Duration::from_secs(5);
        assert_eq!(
            guard.check_at(reading(GB - 122 * MB), &refused, 100 * MB, later),
            Action::Nothing
        );
        assert_eq!(guard.state().0, "watching");
    }

    #[test]
    fn a_worker_scaled_in_rather_than_replaced_is_done_once_it_has_gone() {
        let mut guard = Guard::default();
        let start = Instant::now();
        let workers = [worker(0, 10, 330 * MB), worker(1, 11, 280 * MB)];

        assert!(matches!(
            guard.check_at(reading(900 * MB), &workers, 250 * MB, start),
            Action::Replace { .. }
        ));
        guard.scaled_in();
        assert!(
            guard
                .state()
                .1
                .starts_with("scaling in by shop@r1 worker 0")
        );

        // No successor comes: the worker gone and the room back is enough.

        let gone = [worker(1, 11, 280 * MB)];
        let later = start + Duration::from_secs(5);
        assert_eq!(
            guard.check_at(reading(570 * MB), &gone, 250 * MB, later),
            Action::Nothing
        );
        assert_eq!(guard.state().0, "watching");
    }

    #[test]
    fn nothing_is_judged_before_a_worker_has_reported() {
        let mut guard = Guard::default();
        let unmeasured = [worker(0, 10, 0), worker(1, 11, 0)];

        assert_eq!(
            guard.check(reading(GB - 50 * MB), &unmeasured, 0),
            Action::Nothing
        );
        assert_eq!(guard.state().0, "watching");
    }

    fn limits(cpu_max: Option<(u64, u64)>, memory_max: Option<u64>) -> Limits {
        Limits {
            dir: "/sys/fs/cgroup".into(),
            cpu_max,
            cpus: None,
            memory_max,
        }
    }

    #[test]
    fn more_workers_than_cpus_is_a_warning() {
        let asked = Asked {
            workers: 6,
            groups: 3,
            ..Asked::default()
        };
        let warnings = warnings(&limits(Some((200_000, 100_000)), None), &asked);
        assert_eq!(
            warnings,
            [format!(
                "6 workers over 3 groups is more than the 2 CPUs cpu.max allows; see {GUIDE}"
            )]
        );

        let asked = Asked {
            workers: 2,
            groups: 1,
            ..Asked::default()
        };
        assert!(super::warnings(&limits(Some((200_000, 100_000)), None), &asked).is_empty());
        assert!(super::warnings(&limits(None, None), &asked).is_empty());
    }

    #[test]
    fn a_preload_that_would_not_fit_when_copied_is_a_warning() {
        let asked = Asked {
            workers: 3,
            groups: 1,
            host: Some(250 * MB),
            ..Asked::default()
        };
        assert_eq!(
            warnings(&limits(None, Some(GB)), &asked),
            [format!(
                "the host holds 250MB, and 3 workers that each come to copy it need 1.1GB with \
                 the headroom, more than memory.max 1.0GB; 2 workers fit; see {GUIDE}"
            )]
        );

        let asked = Asked {
            workers: 2,
            ..asked
        };
        assert!(warnings(&limits(None, Some(GB)), &asked).is_empty());
    }

    #[test]
    fn hard_limits_over_the_limit_are_a_warning() {
        let asked = Asked {
            workers: 4,
            groups: 2,
            hard_limits: Some((1200 * MB, 2)),
            ..Asked::default()
        };
        assert_eq!(
            warnings(&limits(None, Some(GB)), &asked),
            [format!(
                "the hard limits of 2 groups, each times its most workers, come to 1.2GB, more \
                 than memory.max 1.0GB; see {GUIDE}"
            )]
        );
    }
}
