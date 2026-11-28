//! Rollouts whose capacity follows traffic (design specification section
//! 9.16, issue 321, ADR 0085 decisions 8 to 13 and ADR 0087): the
//! arithmetic of a total of workers shared between a baseline and a
//! candidate, and what one tick does toward a step's counts. The daemon
//! reads the workers and the budget and carries the moves out; nothing
//! here does either, so the order of a step is tested by itself.

/// The workers the baseline and the candidate each have, of a group's
/// `total`, when the candidate takes `percent` of the traffic.
///
/// The candidate has its share of the total rounded down and the
/// baseline the rest, so a small total rounds in the baseline's favour.
/// Each side with a share of the traffic keeps one worker, which puts
/// the candidate's first worker one over the total until its share
/// earns a whole one, and runs two workers for a total of one. A side
/// with no share is given none here: what a baseline keeps once the
/// candidate is promoted, and a candidate once it is rolled back, is
/// the retention's to say.
pub fn sizes(total: u32, percent: u32) -> (u32, u32) {
    let percent = percent.min(100);
    let earned = (u64::from(total) * u64::from(percent) / 100) as u32;

    let candidate = if percent == 0 { 0 } else { earned.max(1) };

    let baseline = if percent == 100 {
        0
    } else {
        (total - earned).max(1)
    };

    (baseline, candidate)
}

/// One group of a rollout as a tick finds it, on the way to a step's
/// counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group {
    /// The count the baseline's group is pinned to.
    pub baseline: u32,
    /// The count the candidate's group is pinned to.
    pub candidate: u32,
    /// The count the step gives the baseline.
    pub baseline_target: u32,
    /// The count the step gives the candidate.
    pub candidate_target: u32,
    /// The candidate's workers the edge routes to.
    pub candidate_ready: u32,
    /// Whether workers the baseline gave up are still exiting.
    pub baseline_draining: bool,
    /// How many more of the candidate's workers fit the node's budget;
    /// none when nothing limits them.
    pub room: Option<u32>,
    /// The baseline group's `max_unavailable`: how many workers it may
    /// give up ahead of its traffic at a time.
    pub max_unavailable: u32,
}

/// What a tick does for one group before a step's traffic moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// Pin the candidate's group to this count: the budget has room.
    Grow(u32),
    /// Pin the baseline's group down to this count, ahead of the
    /// traffic: the budget has no room for the candidate's workers
    /// until the baseline has given some up.
    Shrink(u32),
    /// Pin the candidate's group down to this count: it has more
    /// workers than its share, having been deployed at its own size.
    Trim(u32),
    /// Nothing yet: the candidate's workers are starting.
    Starting,
    /// Nothing yet: workers the baseline gave up are still exiting.
    Draining,
    /// Nothing can be done: the budget has no room, and the baseline's
    /// `max_unavailable` of 0 lets it give up no worker ahead of its
    /// traffic.
    NoRoom,
    /// The group is where the step wants it, or as near as the budget
    /// allows: `short` when the candidate has fewer workers than its
    /// share, the baseline being down to its count already.
    Ready {
        /// Whether the budget left the candidate short of its count.
        short: bool,
    },
}

/// What to do for one group before a step's traffic moves.
///
/// A candidate with more workers than its share is brought down to it
/// first. With room in the budget the candidate grows, by as many
/// workers as fit. Without, the baseline gives up `max_unavailable`
/// workers, they exit, and the candidate takes the room they leave, in
/// turns until the candidate has its count. Each exchange finishes,
/// the candidate's workers ready and the baseline's gone, before the
/// next begins.
pub fn plan(group: &Group) -> Move {
    if group.candidate > group.candidate_target {
        return Move::Trim(group.candidate_target);
    }

    if group.candidate_ready < group.candidate {
        return Move::Starting;
    }

    if group.candidate >= group.candidate_target {
        return Move::Ready { short: false };
    }

    if group.baseline_draining {
        return Move::Draining;
    }

    let wanted = group.candidate_target - group.candidate;

    match group.room {
        None => Move::Grow(group.candidate_target),
        Some(room) if room >= 1 => Move::Grow(group.candidate + wanted.min(room)),
        Some(_) if group.baseline <= group.baseline_target => Move::Ready { short: true },
        Some(_) if group.max_unavailable == 0 => Move::NoRoom,
        Some(_) => Move::Shrink(
            group
                .baseline
                .saturating_sub(group.max_unavailable)
                .max(group.baseline_target),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_total_is_shared_by_the_candidates_share_rounded_in_the_baselines_favour() {
        // A total of four at steps that are whole workers.

        assert_eq!(sizes(4, 25), (3, 1));
        assert_eq!(sizes(4, 50), (2, 2));
        assert_eq!(sizes(4, 75), (1, 3));

        // A share that earns no whole worker still has one, which is
        // one over the total, and the baseline keeps one to the end.

        assert_eq!(sizes(4, 10), (4, 1));
        assert_eq!(sizes(4, 99), (1, 3));
        assert_eq!(sizes(2, 25), (2, 1));
        assert_eq!(sizes(2, 50), (1, 1));
        assert_eq!(sizes(3, 50), (2, 1));

        // A group of one runs two for the length of the rollout.

        assert_eq!(sizes(1, 1), (1, 1));
        assert_eq!(sizes(1, 99), (1, 1));

        // With all the traffic or none, the other side is left to its
        // retention.

        assert_eq!(sizes(4, 100), (0, 4));
        assert_eq!(sizes(4, 0), (4, 0));
        assert_eq!(sizes(4, 250), (0, 4));
    }

    fn group(baseline: u32, candidate: u32, targets: (u32, u32)) -> Group {
        Group {
            baseline,
            candidate,
            baseline_target: targets.0,
            candidate_target: targets.1,
            candidate_ready: candidate,
            baseline_draining: false,
            room: None,
            max_unavailable: 0,
        }
    }

    #[test]
    fn with_room_the_candidate_grows_before_the_traffic_moves() {
        // No budget: straight to its count, then nothing until its
        // workers are ready, then ready.

        let step = group(3, 1, (2, 2));
        assert_eq!(plan(&step), Move::Grow(2));

        let grown = Group {
            candidate: 2,
            candidate_ready: 1,
            ..step
        };
        assert_eq!(plan(&grown), Move::Starting);
        assert_eq!(
            plan(&Group {
                candidate_ready: 2,
                ..grown
            }),
            Move::Ready { short: false }
        );

        // A budget with room for some: as many as fit, then the rest.

        let wide = Group {
            room: Some(1),
            ..group(4, 1, (1, 3))
        };
        assert_eq!(plan(&wide), Move::Grow(2));
        assert_eq!(
            plan(&Group {
                room: Some(5),
                ..wide
            }),
            Move::Grow(3)
        );
    }

    #[test]
    fn without_room_the_baseline_gives_up_workers_first_max_unavailable_at_a_time() {
        let full = Group {
            room: Some(0),
            max_unavailable: 1,
            ..group(3, 1, (1, 3))
        };
        assert_eq!(plan(&full), Move::Shrink(2));

        // The worker it gave up exits, the room appears, the candidate
        // takes it, and the next exchange waits for that worker.

        let gave = Group {
            baseline: 2,
            baseline_draining: true,
            ..full
        };
        assert_eq!(plan(&gave), Move::Draining);

        let room = Group {
            baseline_draining: false,
            room: Some(1),
            ..gave
        };
        assert_eq!(plan(&room), Move::Grow(2));

        let starting = Group {
            candidate: 2,
            candidate_ready: 1,
            room: Some(0),
            ..room
        };
        assert_eq!(plan(&starting), Move::Starting);
        assert_eq!(
            plan(&Group {
                candidate_ready: 2,
                ..starting
            }),
            Move::Shrink(1)
        );

        // More at a time with a larger max_unavailable, and never
        // under the step's count.

        assert_eq!(
            plan(&Group {
                max_unavailable: 5,
                ..full
            }),
            Move::Shrink(1)
        );
    }

    #[test]
    fn a_candidate_over_its_share_is_brought_down_to_it() {
        // Deployed held at its own four workers, it has one at 25%, and
        // the step goes on while the others leave.

        let full = group(4, 4, (3, 1));
        assert_eq!(plan(&full), Move::Trim(1));
        assert_eq!(
            plan(&Group {
                candidate: 1,
                ..full
            }),
            Move::Ready { short: false }
        );
    }

    #[test]
    fn without_room_and_nothing_to_give_up_the_step_waits_or_goes_short() {
        // The default max_unavailable lets the baseline give up nothing
        // ahead of its traffic.

        let stuck = Group {
            room: Some(0),
            ..group(3, 1, (2, 2))
        };
        assert_eq!(plan(&stuck), Move::NoRoom);

        // The baseline is down to its count and there is still no room:
        // the budget lowers the total, and the candidate goes on with
        // what it has.

        let short = Group {
            room: Some(0),
            max_unavailable: 1,
            ..group(2, 1, (2, 2))
        };
        assert_eq!(plan(&short), Move::Ready { short: true });
    }
}
