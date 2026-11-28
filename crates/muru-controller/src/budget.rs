//! Section 7.4: the node budget manager. The `[daemon.budget]` table caps
//! the workers, memory and CPU the releases on the node hold between
//! them; scale-ups are admitted against it, the highest application
//! priority first, and a release starts only when its memory fits.

use std::collections::BTreeMap;
use std::fmt;

use muru_config::format_size_approx;

use crate::model::ReleaseSpec;

/// The node's budget; a limit of none is off.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Budget {
    /// Worker processes and managed server instances.
    pub workers: Option<u32>,
    /// Bytes of memory for the hosts and workers.
    pub memory: Option<u64>,
    /// CPU cores for the workers.
    pub cpu: Option<f64>,
}

impl Budget {
    /// The budget a `[daemon.budget]` table sets, checked.
    pub fn from_table(table: Option<&muru_config::Budget>) -> Result<Self, String> {
        let Some(table) = table else {
            return Ok(Self::default());
        };

        if table.workers == Some(0) {
            return Err("workers must be at least 1".to_owned());
        }

        let memory = table
            .memory
            .as_deref()
            .map(muru_config::parse_size)
            .transpose()?;

        if memory == Some(0) {
            return Err("memory must be more than 0".to_owned());
        }

        if table.cpu.is_some_and(|cpu| !cpu.is_finite() || cpu <= 0.0) {
            return Err("cpu must be a number of cores above 0".to_owned());
        }

        Ok(Self {
            workers: table.workers,
            memory,
            cpu: table.cpu,
        })
    }

    /// Whether any limit is set.
    pub fn is_set(&self) -> bool {
        self.workers.is_some() || self.memory.is_some() || self.cpu.is_some()
    }

    /// What is held against each limit that is set, such as "12 of 16
    /// workers, 5.1GB of 8GB memory".
    pub fn describe(&self, used: &Usage) -> String {
        let mut parts = Vec::new();

        if let Some(workers) = self.workers {
            parts.push(format!("{} of {workers} workers", used.workers));
        }

        if let Some(memory) = self.memory {
            parts.push(format!(
                "{} of {} memory",
                format_size_approx(used.memory),
                format_size_approx(memory)
            ));
        }

        if let Some(cpu) = self.cpu {
            parts.push(format!("{:.1} of {cpu} cores", used.cpu));
        }

        // Section 7.9: the room held for swaps is part of what is held.

        if used.surge > 0 && !parts.is_empty() {
            parts.push(format!(
                "{} held for swaps",
                if used.surge == 1 {
                    "1 worker".to_owned()
                } else {
                    format!("{} workers", used.surge)
                }
            ));
        }

        if parts.is_empty() {
            "no limits".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

/// What one worker costs, measured or estimated; none where unknown,
/// which that limit then cannot hold back.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    /// Bytes of memory.
    pub memory: Option<u64>,
    /// CPU cores.
    pub cpu: Option<f64>,
}

/// What the node's releases hold.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// Workers and instances.
    pub workers: u32,
    /// Bytes of memory.
    pub memory: u64,
    /// CPU cores.
    pub cpu: f64,
    /// How many of the workers are surge allowances (section 7.9): room
    /// held for a swap, with no worker in it between swaps.
    pub surge: u32,
}

impl Usage {
    /// Add `count` workers of `cost`.
    pub fn add(&mut self, count: u32, cost: Cost) {
        self.workers += count;
        self.memory += cost.memory.unwrap_or(0) * count as u64;
        self.cpu += cost.cpu.unwrap_or(0.0) * count as f64;
    }

    /// Add a surge allowance of `count` workers of `cost`.
    pub fn add_surge(&mut self, count: u32, cost: Cost) {
        self.add(count, cost);
        self.surge += count;
    }

    /// Add what another holds.
    pub fn add_usage(&mut self, other: &Usage) {
        self.workers += other.workers;
        self.memory += other.memory;
        self.cpu += other.cpu;
        self.surge += other.surge;
    }
}

/// Which limit held a scale-up back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// The workers.
    Workers,
    /// The memory.
    Memory,
    /// The CPU cores.
    Cpu,
}

impl fmt::Display for Limit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Limit::Workers => "worker",
            Limit::Memory => "memory",
            Limit::Cpu => "CPU",
        })
    }
}

/// How many more workers of `cost` fit the budget over `used`, and the
/// limit that sets that, or none when nothing limits them.
pub fn room(budget: &Budget, used: &Usage, cost: Cost) -> Option<(u32, Limit)> {
    let mut least: Option<(u32, Limit)> = None;
    let mut consider = |fits: u64, limit: Limit| {
        let fits = u32::try_from(fits).unwrap_or(u32::MAX);

        if least.is_none_or(|(fewest, _)| fits < fewest) {
            least = Some((fits, limit));
        }
    };

    if let Some(workers) = budget.workers {
        consider(workers.saturating_sub(used.workers).into(), Limit::Workers);
    }

    if let (Some(memory), Some(each)) = (budget.memory, cost.memory)
        && each > 0
    {
        consider(memory.saturating_sub(used.memory) / each, Limit::Memory);
    }

    if let (Some(cpu), Some(each)) = (budget.cpu, cost.cpu)
        && each > 0.0
    {
        consider(
            ((cpu - used.cpu).max(0.0) / each).floor() as u64,
            Limit::Cpu,
        );
    }

    least
}

/// One group's scale-up, for admission.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask<K> {
    /// Which group.
    pub key: K,
    /// Its application's priority.
    pub priority: i32,
    /// The count it holds.
    pub from: u32,
    /// The count it asks for.
    pub to: u32,
    /// What each worker costs.
    pub cost: Cost,
}

/// What admission gave a scale-up.
#[derive(Debug, Clone, PartialEq)]
pub struct Admitted<K> {
    /// The scale-up.
    pub ask: Ask<K>,
    /// The count admitted.
    pub to: u32,
    /// The limit that admitted fewer than asked for, when one did.
    pub limit: Option<Limit>,
}

/// Admit scale-ups against the budget over what the node holds without
/// them: the highest priority first, in key order within a priority,
/// each given as many of the workers it asks for as still fit.
pub fn admit<K: Ord>(budget: &Budget, mut used: Usage, mut asks: Vec<Ask<K>>) -> Vec<Admitted<K>> {
    asks.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.key.cmp(&b.key)));

    asks.into_iter()
        .map(|ask| {
            let wanted = ask.to.saturating_sub(ask.from);

            let (fits, limit) = match room(budget, &used, ask.cost) {
                Some((fits, limit)) if fits < wanted => (fits, Some(limit)),
                _ => (wanted, None),
            };

            used.add(fits, ask.cost);
            Admitted {
                to: ask.from + fits,
                limit,
                ask,
            }
        })
        .collect()
}

/// Why a group's surge allowance is less than its `max_surge` (section
/// 7.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Short {
    /// Its release takes no share of its application's traffic.
    Traffic,
    /// The budget has no room, by this limit.
    Budget(Limit),
}

impl Short {
    /// One word for the host and the admin API: `traffic`, `workers`,
    /// `memory` or `cpu`.
    pub fn as_str(self) -> &'static str {
        match self {
            Short::Traffic => "traffic",
            Short::Budget(Limit::Workers) => "workers",
            Short::Budget(Limit::Memory) => "memory",
            Short::Budget(Limit::Cpu) => "cpu",
        }
    }
}

/// A group's surge allowance (section 7.9): how many workers it may run
/// over its count during a swap, and why that is less than its
/// `max_surge` when it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Surge {
    /// The workers allowed over the count.
    pub allowance: u32,
    /// What left it short of the group's `max_surge`.
    pub short: Option<Short>,
}

/// One group's wish for a surge allowance.
#[derive(Debug, Clone, PartialEq)]
pub struct SurgeAsk<K> {
    /// Which group.
    pub key: K,
    /// Its application's priority.
    pub priority: i32,
    /// The group's `max_surge`.
    pub max_surge: u32,
    /// What each worker costs.
    pub cost: Cost,
    /// Whether its release takes a share of its application's traffic.
    pub traffic: bool,
}

/// Grant each group its surge allowance over what the node holds without
/// any (section 7.9): none for a release that takes no traffic, and for
/// the rest as much of `max_surge` as the budget still has room for,
/// admitted as scale-ups are, the highest priority first and in key
/// order within one. With no budget every group that takes traffic has
/// its `max_surge`.
pub fn grant_surges<K: Ord + Clone>(
    budget: &Budget,
    used: Usage,
    asks: Vec<SurgeAsk<K>>,
) -> BTreeMap<K, Surge> {
    let mut granted = BTreeMap::new();
    let mut admitted = Vec::new();

    for ask in asks {
        if ask.traffic {
            admitted.push(Ask {
                key: ask.key,
                priority: ask.priority,
                from: 0,
                to: ask.max_surge,
                cost: ask.cost,
            });
        } else {
            granted.insert(
                ask.key,
                Surge {
                    allowance: 0,
                    short: (ask.max_surge > 0).then_some(Short::Traffic),
                },
            );
        }
    }

    for given in admit(budget, used, admitted) {
        granted.insert(
            given.ask.key,
            Surge {
                allowance: given.to,
                short: given.limit.map(Short::Budget),
            },
        );
    }

    granted
}

/// Whether bringing a group from `held` to `floor` workers of `cost`
/// fits the budget over `used`, which counts the `held`; why not when it
/// does not.
pub fn fits(
    budget: &Budget,
    used: &Usage,
    held: u32,
    floor: u32,
    cost: Cost,
) -> Result<(), String> {
    let wanted = floor.saturating_sub(held);

    match room(budget, used, cost) {
        Some((fits, limit)) if fits < wanted => Err(format!(
            "{wanted} more workers are needed and the node's {limit} budget has room for {fits} ({})",
            budget.describe(used)
        )),
        _ => Ok(()),
    }
}

/// The node's budget and what its releases hold as the daemon last
/// measured them, for the checks made before a release starts, when a
/// schedule is applied and ahead of its windows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct View {
    /// The budget.
    pub budget: Budget,
    /// What the running releases hold.
    pub used: Usage,
    /// The workers each group of each application's serving release
    /// holds and what one costs, by application and group.
    pub groups: BTreeMap<(String, String), (u32, Cost)>,
    /// The memory of each application's serving host.
    pub hosts: BTreeMap<String, u64>,
    /// The mean memory of every worker on the node, when any reported.
    pub fallback: Option<u64>,
    /// The memory the retained releases hold, which they give back to
    /// make room for a release.
    pub retained: u64,
    /// Where the limits the daemon file leaves out come from, such as
    /// ` (memory from memory.max 2.0GB less 205MB of headroom)`, or empty.
    pub sources: String,
    /// Whether the memory limit is the daemon's cgroup's, the daemon's
    /// file setting none (issue 961).
    pub memory_from_cgroup: bool,
}

impl View {
    /// What a worker of an application's group costs: the hard limit
    /// `spec` sets for it, the user's own statement of how large a worker
    /// may grow (issue 384), else its serving release's measure, else the
    /// soft limit, else the node's mean.
    fn cost(
        &self,
        app: &str,
        group: &str,
        hard_limit: Option<u64>,
        soft_limit: Option<u64>,
    ) -> Cost {
        let measured = self
            .groups
            .get(&(app.to_owned(), group.to_owned()))
            .map(|(_, cost)| *cost)
            .unwrap_or_default();

        Cost {
            memory: hard_limit
                .or(measured.memory)
                .or(soft_limit)
                .or(self.fallback),
            cpu: measured.cpu,
        }
    }

    /// The memory a new release of `app` would take: its host and its
    /// workers at their starting counts, each worker at its group's hard
    /// limit when one is set, else what the application's serving
    /// release measures, else the soft limit, else
    /// the node's mean, and the host at what the serving host measures,
    /// else a worker's. None when nothing is known, and for a managed
    /// server, whose memory is not measured.
    pub fn release_memory(&self, app: &str, spec: &ReleaseSpec) -> Option<u64> {
        if spec.http.is_some() {
            return None;
        }

        let soft_limit = spec.memory.soft_limit;
        let groups = std::iter::once((
            muru_edge::routing::DEFAULT_GROUP,
            spec.starting_processes(),
            spec.memory.hard_limit,
        ))
        .chain(spec.groups.iter().map(|group| {
            let hard = group
                .memory
                .as_ref()
                .map_or(spec.memory.hard_limit, |memory| memory.hard_limit);
            (group.name.as_str(), group.starting_processes(), hard)
        }));
        let mut total = 0;
        let mut each = None;

        for (group, count, hard_limit) in groups {
            let memory = self.cost(app, group, hard_limit, soft_limit).memory?;
            each.get_or_insert(memory);
            total += memory * count as u64;
        }

        Some(total + self.hosts.get(app).copied().or(each)?)
    }

    /// Whether a new release of `app` fits the memory budget over what
    /// the node holds and what `freed` would give back; why not when it
    /// does not.
    pub fn release_fits(&self, app: &str, spec: &ReleaseSpec, freed: u64) -> Result<(), String> {
        let (Some(memory), Some(needed)) = (self.budget.memory, self.release_memory(app, spec))
        else {
            return Ok(());
        };

        let held = self.used.memory.saturating_sub(freed);

        if held + needed <= memory {
            return Ok(());
        }

        Err(format!(
            "the release needs about {} and the node's memory budget of {} has {} held",
            format_size_approx(needed),
            format_size_approx(memory),
            format_size_approx(held)
        ))
    }

    /// Whether raising an application's group to `floor` workers fits the
    /// budget over what the node holds; why not when it does not.
    pub fn floor_fits(
        &self,
        app: &str,
        group: &str,
        floor: u32,
        soft_limit: Option<u64>,
    ) -> Result<(), String> {
        let held = self
            .groups
            .get(&(app.to_owned(), group.to_owned()))
            .map_or(0, |(held, _)| *held);
        fits(
            &self.budget,
            &self.used,
            held,
            floor,
            self.cost(app, group, None, soft_limit),
        )
    }
}

/// A process's proportional set size, from /proc on Linux; none
/// elsewhere or when it cannot be read.
pub fn process_pss(pid: u32) -> Option<u64> {
    if !cfg!(target_os = "linux") {
        return None;
    }

    let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    let line = text.lines().find(|line| line.starts_with("Pss:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn a_worker_counts_at_its_hard_limit_before_what_it_measures() {
        let mut view = View {
            fallback: Some(50 * MB),
            ..View::default()
        };
        view.groups.insert(
            ("shop".to_owned(), "default".to_owned()),
            (
                2,
                Cost {
                    memory: Some(120 * MB),
                    cpu: None,
                },
            ),
        );

        let cost = |hard, soft| view.cost("shop", "default", hard, soft).memory;
        assert_eq!(cost(Some(300 * MB), Some(80 * MB)), Some(300 * MB));
        assert_eq!(cost(None, Some(80 * MB)), Some(120 * MB));

        let unmeasured = |hard, soft| view.cost("shop", "reports", hard, soft).memory;
        assert_eq!(unmeasured(None, Some(80 * MB)), Some(80 * MB));
        assert_eq!(unmeasured(None, None), Some(50 * MB));
    }

    // Issue 964: a release deployed with an initial count is checked at
    // it, where it was checked at the count it is declared with.

    #[test]
    fn a_release_is_checked_at_the_count_it_starts_with() {
        let view = View {
            budget: Budget {
                memory: Some(1000 * MB),
                ..Budget::default()
            },
            used: Usage {
                memory: 600 * MB,
                ..Usage::default()
            },
            fallback: Some(200 * MB),
            ..View::default()
        };
        let mut spec = crate::store::tests::spec();
        spec.processes = 4;

        // Four workers and a host, each at the node's mean.

        assert_eq!(view.release_memory("shop", &spec), Some(1000 * MB));
        assert!(view.release_fits("shop", &spec, 0).is_err());

        spec.initial_processes = Some(1);
        assert_eq!(view.release_memory("shop", &spec), Some(400 * MB));
        assert_eq!(view.release_fits("shop", &spec, 0), Ok(()));
    }

    fn ask(key: &str, priority: i32, from: u32, to: u32, memory: u64) -> Ask<String> {
        Ask {
            key: key.to_owned(),
            priority,
            from,
            to,
            cost: Cost {
                memory: Some(memory),
                cpu: None,
            },
        }
    }

    #[test]
    fn surge_allowances_are_granted_within_the_budget_and_only_with_traffic() {
        let surge = |key: &str, priority, max_surge, traffic| SurgeAsk {
            key: key.to_owned(),
            priority,
            max_surge,
            cost: Cost {
                memory: Some(100 * MB),
                cpu: None,
            },
            traffic,
        };
        let asks = || {
            vec![
                surge("blog", 0, 1, true),
                surge("held", 5, 1, false),
                surge("off", 0, 0, false),
                surge("shop", 5, 2, true),
            ]
        };

        // Section 7.9: with no budget, a group that takes traffic has
        // its max_surge, and one that takes none has no allowance.

        let free = grant_surges(&Budget::default(), Usage::default(), asks());
        assert_eq!(
            free["shop"],
            Surge {
                allowance: 2,
                short: None
            }
        );
        assert_eq!(
            free["blog"],
            Surge {
                allowance: 1,
                short: None
            }
        );
        assert_eq!(
            free["held"],
            Surge {
                allowance: 0,
                short: Some(Short::Traffic)
            }
        );
        assert_eq!(free["off"], Surge::default());

        // With room for two more workers the higher priority is granted
        // first, and the other is told which limit left it none.

        let budget = Budget {
            memory: Some(600 * MB),
            ..Budget::default()
        };
        let used = Usage {
            workers: 4,
            memory: 400 * MB,
            ..Usage::default()
        };
        let tight = grant_surges(&budget, used, asks());
        assert_eq!(
            tight["shop"],
            Surge {
                allowance: 2,
                short: None
            }
        );
        assert_eq!(
            tight["blog"],
            Surge {
                allowance: 0,
                short: Some(Short::Budget(Limit::Memory))
            }
        );
        assert_eq!(Short::Budget(Limit::Memory).as_str(), "memory");
        assert_eq!(Short::Traffic.as_str(), "traffic");

        // What is granted counts as workers held, and is named.

        let mut held = used;
        held.add_surge(
            2,
            Cost {
                memory: Some(100 * MB),
                cpu: None,
            },
        );
        assert_eq!(
            budget.describe(&held),
            "600MB of 600MB memory, 2 workers held for swaps"
        );
    }

    #[test]
    fn the_table_is_checked() {
        let table = muru_config::Budget {
            workers: Some(16),
            memory: Some("8GB".to_owned()),
            cpu: Some(6.0),
        };
        let budget = Budget::from_table(Some(&table)).unwrap();
        assert_eq!(budget.memory, Some(8 * 1024 * MB));
        assert!(budget.is_set());
        assert!(!Budget::from_table(None).unwrap().is_set());

        for bad in [
            muru_config::Budget {
                workers: Some(0),
                ..Default::default()
            },
            muru_config::Budget {
                memory: Some("lots".to_owned()),
                ..Default::default()
            },
            muru_config::Budget {
                cpu: Some(0.0),
                ..Default::default()
            },
        ] {
            assert!(Budget::from_table(Some(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn higher_priorities_are_admitted_first_and_the_rest_in_part() {
        let budget = Budget {
            workers: Some(10),
            memory: Some(1000 * MB),
            cpu: None,
        };
        let used = Usage {
            workers: 6,
            memory: 600 * MB,
            cpu: 0.0,
            surge: 0,
        };
        let admitted = admit(
            &budget,
            used,
            vec![
                ask("blog", 0, 2, 5, 100 * MB),
                ask("shop", 10, 4, 6, 100 * MB),
            ],
        );

        assert_eq!(admitted[0].ask.key, "shop");
        assert_eq!((admitted[0].to, admitted[0].limit), (6, None));
        assert_eq!(admitted[1].ask.key, "blog");
        assert_eq!(
            (admitted[1].to, admitted[1].limit),
            (4, Some(Limit::Workers))
        );
    }

    #[test]
    fn memory_and_cpu_limit_by_what_a_worker_costs() {
        let budget = Budget {
            workers: None,
            memory: Some(1000 * MB),
            cpu: Some(4.0),
        };
        let used = Usage {
            workers: 4,
            memory: 700 * MB,
            cpu: 1.0,
            surge: 0,
        };
        let cost = Cost {
            memory: Some(200 * MB),
            cpu: Some(0.5),
        };
        assert_eq!(room(&budget, &used, cost), Some((1, Limit::Memory)));

        let cost = Cost {
            memory: Some(10 * MB),
            cpu: Some(1.0),
        };
        assert_eq!(room(&budget, &used, cost), Some((3, Limit::Cpu)));

        // Nothing known about a worker: only the count could hold it.

        assert_eq!(room(&budget, &used, Cost::default()), None);
        assert!(fits(&budget, &used, 2, 3, cost).is_ok());

        let refused = fits(&budget, &used, 2, 8, cost).unwrap_err();
        assert!(refused.contains("6 more workers"), "{refused}");
        assert!(refused.contains("CPU budget has room for 3"), "{refused}");
    }
}
