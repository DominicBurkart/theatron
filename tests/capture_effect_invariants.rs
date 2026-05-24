//! Multi-way capture-effect invariants for [`Channel::begin_transmission`]
//! and its propagation through the scheduler.
//!
//! These tests cover behaviour that existing channel tests only pin down for
//! the 2-TX case. The current implementation marks pairwise interactions
//! between each new arrival and every overlapping active transmission. That
//! design has subtle consequences in the 3+ TX case that user-facing
//! consumers of [`Channel::drain_completed`] depend on but which are not
//! otherwise asserted anywhere:
//!
//! 1. **Dominant survival.** When one signal dominates *every* overlapping
//!    transmission by at least the capture threshold, that signal is the
//!    unique survivor — regardless of arrival order.
//!
//! 2. **Mutual collision.** When no signal dominates every overlapper, every
//!    overlapping transmission ends up flagged `collided` and is not
//!    delivered.
//!
//! 3. **Delivery / collision exclusivity (user-facing contract).** Every
//!    completed transmission delivered to the scheduler is either delivered
//!    once (counted in `total_rx`) or counted once in `total_collisions` —
//!    never both, never neither.
//!
//! 4. **`captured` is only meaningful when `!collided`.** Because
//!    [`Channel::begin_transmission`] sets `active.captured = true` on a
//!    previously inserted (now-weaker) signal at the moment the comparison
//!    is made, a *third* even-stronger arrival can subsequently mark that
//!    same TX `collided = true`. The scheduler-side consumer correctly
//!    treats the `collided` flag as authoritative (see
//!    `deliver_completed_to_nodes`), and `total_captures` only increments
//!    when `!collided && captured`. This file pins that behaviour so a
//!    refactor of the capture flag cannot silently inflate the capture
//!    metric.
//!
//! 5. **Capture metric accounting.** `total_captures` is incremented at most
//!    once per delivered transmission, never per receiver. With N receivers
//!    and one captured TX, `total_captures == 1` and `total_rx == N - 1`
//!    (the sender does not receive its own frame).
//!
//! A regression in any of these invariants could silently change the
//! delivered traffic rate or the capture metric of every LoRa simulation,
//! while leaving the simpler 2-TX channel tests passing.

use std::collections::HashSet;

use proptest::prelude::*;
use theatron::channel::{Channel, ChannelConfig};
use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::types::{NodeId, RxMetadata, Transmission};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tx_at_power(power_dbm: i8) -> Transmission {
    Transmission {
        payload: vec![power_dbm as u8],
        sf: 7,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency: 868_100_000,
        duration_us: 50_000,
        tx_power_dbm: power_dbm,
    }
}

/// A scheduler-level node that fires a single configured TX on first
/// transmit poll, then stays silent.
struct OneShot {
    id: NodeId,
    pending: Option<Transmission>,
}

impl OneShot {
    fn new(id: u32, t: Transmission) -> Self {
        Self {
            id: NodeId(id),
            pending: Some(t),
        }
    }
}

impl NodeHandle for OneShot {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        self.pending.take()
    }
    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

/// A pure receiver that records every frame it gets.
struct Listener {
    id: NodeId,
}

impl Listener {
    fn new(id: u32) -> Self {
        Self { id: NodeId(id) }
    }
}

impl NodeHandle for Listener {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        None
    }
    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

/// Helper: insert a sequence of `(sender, power, start_us)` transmissions
/// into a fresh LoRa channel, then resolve and drain.
///
/// All transmissions share SF, frequency and duration so they form a single
/// overlap class. Returns `(sender, collided, captured)` triples in the
/// order returned by `drain_completed`.
fn run_window(arrivals: &[(u32, i8, SimTime)], duration_us: u64) -> Vec<(NodeId, bool, bool)> {
    let mut ch = Channel::new();
    let mut latest_end = 0u64;
    for &(id, power, start) in arrivals {
        let mut t = tx_at_power(power);
        t.duration_us = duration_us;
        ch.begin_transmission(NodeId(id), &t, start);
        latest_end = latest_end.max(start + duration_us);
    }
    ch.resolve_at(latest_end);
    ch.drain_completed()
        .into_iter()
        .map(|(s, c, cap, _)| (s, c, cap))
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Dominant-survival in a 3-way overlap
// ---------------------------------------------------------------------------

/// When one TX is at least `threshold` dB stronger than every overlapping TX,
/// it is the unique survivor — and the survivor is flagged `captured`.
#[test]
fn unique_dominator_survives_three_way_overlap() {
    // LoRa default threshold = 6 dB. Strong=20 dominates medium=14 and weak=8
    // by 6 dB and 12 dB respectively.
    let completed = run_window(&[(1, 20, 0), (2, 14, 5_000), (3, 8, 10_000)], 50_000);

    let survivors: Vec<_> = completed.iter().filter(|(_, c, _)| !c).collect();
    assert_eq!(survivors.len(), 1, "exactly one survivor in 3-way overlap");
    assert_eq!(survivors[0].0, NodeId(1), "strongest is the survivor");
    assert!(survivors[0].2, "survivor must be flagged captured");
}

/// Dominator survival is independent of arrival order: shuffling the start
/// times of the three TXs keeps the strongest as the unique survivor.
#[test]
fn dominator_survives_regardless_of_arrival_order() {
    // Six permutations of (id, power) -> assigned to three start times.
    let powers = [(1u32, 20i8), (2u32, 14i8), (3u32, 8i8)];
    let starts = [0u64, 5_000, 10_000];

    let permutations: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];

    for perm in &permutations {
        let arrivals = [
            (powers[0].0, powers[0].1, starts[perm[0]]),
            (powers[1].0, powers[1].1, starts[perm[1]]),
            (powers[2].0, powers[2].1, starts[perm[2]]),
        ];
        let completed = run_window(&arrivals, 50_000);
        let survivors: Vec<_> = completed.iter().filter(|(_, c, _)| !c).collect();
        assert_eq!(
            survivors.len(),
            1,
            "permutation {:?} produced {} survivors",
            perm,
            survivors.len(),
        );
        assert_eq!(
            survivors[0].0,
            NodeId(1),
            "permutation {:?}: strongest must still be the survivor",
            perm,
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Mutual collision when no signal dominates
// ---------------------------------------------------------------------------

/// When the spread of powers is below the threshold, no signal dominates and
/// every overlapping TX is marked collided.
#[test]
fn no_dominator_all_collide() {
    // Powers 14, 12, 10: max delta is 4 dB, below the 6 dB threshold.
    let completed = run_window(&[(1, 14, 0), (2, 12, 1_000), (3, 10, 2_000)], 50_000);
    assert_eq!(completed.len(), 3);
    assert!(
        completed.iter().all(|(_, c, _)| *c),
        "no dominator -> all collide: {:?}",
        completed,
    );
}

/// A non-dominating "strong" signal collides too: stronger by `threshold`
/// than ONE overlapper but not by `threshold` against the other.
#[test]
fn partial_dominator_does_not_survive() {
    // Powers 20, 14, 16: 20 vs 14 -> delta=6 (>=threshold). 20 vs 16 ->
    // delta=4 (middle case: both collide). So 20 ends up collided.
    let completed = run_window(&[(1, 20, 0), (2, 14, 1_000), (3, 16, 2_000)], 50_000);
    assert_eq!(completed.len(), 3);
    let survivors: Vec<_> = completed.iter().filter(|(_, c, _)| !c).collect();
    assert!(
        survivors.is_empty(),
        "no signal dominates ALL overlappers -> no survivor: {:?}",
        completed,
    );
}

// ---------------------------------------------------------------------------
// 3. & 4. Delivery / collision exclusivity & `captured` semantics
// ---------------------------------------------------------------------------

/// Scheduler-level invariant: every completed TX is either delivered (counted
/// in `total_rx`) or counted in `total_collisions`, never both, never neither.
/// And `total_captures` is bounded by `total_tx - total_collisions`.
#[test]
fn metrics_partition_completed_transmissions() {
    // Three senders, two listeners, all on the same SF/freq.
    let mut sched = Scheduler::new(500_000);
    sched.add_node(Box::new(OneShot::new(1, tx_at_power(20))), Some(0));
    sched.add_node(Box::new(OneShot::new(2, tx_at_power(14))), Some(5_000));
    sched.add_node(Box::new(OneShot::new(3, tx_at_power(8))), Some(10_000));
    sched.add_node(Box::new(Listener::new(10)), None);
    sched.add_node(Box::new(Listener::new(11)), None);
    sched.run();

    let total_tx = sched.metrics.total_tx;
    let total_collisions = sched.metrics.total_collisions;
    let total_rx = sched.metrics.total_rx;
    let total_captures = sched.metrics.total_captures;
    assert_eq!(total_tx, 3);

    // Each completed TX is either delivered to every non-sender or counted
    // as a collision. With 3 senders + 2 listeners = 5 nodes, each
    // delivered TX reaches 5 - 1 = 4 non-sender nodes.
    let delivered_count = total_tx - total_collisions;
    assert_eq!(
        total_rx,
        delivered_count * 4,
        "delivered TXs are broadcast to all non-sender nodes (4 each)"
    );

    // captured is bounded by delivered.
    assert!(
        total_captures <= delivered_count,
        "captures ({}) cannot exceed delivered ({})",
        total_captures,
        delivered_count,
    );
}

/// `total_captures` counts each captured TX exactly once, *not* once per
/// receiver. With N listeners and one captured TX, `total_captures == 1`.
#[test]
fn total_captures_is_per_transmission_not_per_receiver() {
    let mut sched = Scheduler::new(500_000);
    sched.add_node(Box::new(OneShot::new(1, tx_at_power(20))), Some(0));
    sched.add_node(Box::new(OneShot::new(2, tx_at_power(14))), Some(5_000));
    // Five passive listeners.
    for i in 10..15 {
        sched.add_node(Box::new(Listener::new(i)), None);
    }
    sched.run();

    // Strong should be captured exactly once even though it reaches many
    // receivers; weak should be collided.
    assert_eq!(sched.metrics.total_tx, 2);
    assert_eq!(sched.metrics.total_collisions, 1);
    assert_eq!(
        sched.metrics.total_captures, 1,
        "captures must be per-TX, not per-receiver"
    );
    // 6 non-sender nodes for the strong sender (weak + 5 listeners),
    // 0 deliveries for the weak (it was collided).
    assert_eq!(sched.metrics.total_rx, 6);
}

/// A captured-then-collided transmission is treated as collided by the
/// scheduler (the `collided` flag is authoritative) and does not increment
/// `total_captures`.
///
/// Construction: strong=20 arrives first, then weak=14 (strong dominates
/// weak by 6 dB so weak.collided=true, strong.captured=true). Then a third
/// arrival super=27 dominates strong by 7 dB, so strong.collided=true while
/// strong.captured stays true (the channel does not clear it).
///
/// The scheduler must:
///   * deliver `super` (it dominates everyone) and count one capture;
///   * NOT count an extra capture for `strong` even though its `captured`
///     flag is still true;
///   * mark both `strong` and `weak` as collisions.
#[test]
fn captured_then_collided_does_not_inflate_captures() {
    let mut sched = Scheduler::new(500_000);
    sched.add_node(Box::new(OneShot::new(1, tx_at_power(20))), Some(0));
    sched.add_node(Box::new(OneShot::new(2, tx_at_power(14))), Some(2_000));
    // super: 27 dBm dominates strong (20) by 7 dB and weak (14) by 13 dB.
    sched.add_node(Box::new(OneShot::new(3, tx_at_power(27))), Some(4_000));
    sched.add_node(Box::new(Listener::new(10)), None);
    sched.run();

    assert_eq!(sched.metrics.total_tx, 3);
    assert_eq!(
        sched.metrics.total_collisions, 2,
        "strong and weak both end up collided"
    );
    assert_eq!(
        sched.metrics.total_captures, 1,
        "exactly one capture (the super TX), not two"
    );
    // super reaches 3 non-sender nodes (strong, weak, listener).
    assert_eq!(sched.metrics.total_rx, 3);
}

// ---------------------------------------------------------------------------
// 5. Single dominator delivered to many receivers
// ---------------------------------------------------------------------------

/// Property: with one dominator and N collided weaker TXs, the dominator is
/// delivered to exactly (N + listeners) non-sender nodes (everyone except
/// itself).
#[test]
fn dominator_broadcasts_to_all_non_senders() {
    let mut sched = Scheduler::new(500_000);
    sched.add_node(Box::new(OneShot::new(1, tx_at_power(25))), Some(0));
    // Four weak senders, each colliding with each other and dominated by
    // the strong sender.
    for i in 2..=5 {
        sched.add_node(
            Box::new(OneShot::new(i, tx_at_power(10))),
            Some(1_000 * i as u64),
        );
    }
    // Three passive listeners.
    for i in 10..13 {
        sched.add_node(Box::new(Listener::new(i)), None);
    }
    sched.run();

    assert_eq!(sched.metrics.total_tx, 5, "5 senders");
    // 4 weak collide with each other (within threshold of each other) and
    // with the strong, but the strong dominates each of them.
    assert_eq!(sched.metrics.total_captures, 1);
    assert_eq!(sched.metrics.total_collisions, 4);
    // Strong reaches 4 weak senders + 3 listeners = 7 receivers.
    assert_eq!(sched.metrics.total_rx, 7);
}

// ---------------------------------------------------------------------------
// Non-overlap edge: a strictly-later TX is independent
// ---------------------------------------------------------------------------

/// A transmission that begins exactly when an active transmission ends does
/// not overlap (strict inequality in the channel's `overlaps`). It is
/// delivered as a clean frame even though its `start` equals the previous
/// `end`.
#[test]
fn adjacent_transmissions_in_three_way_chain_no_collision() {
    // Three back-to-back TXs, each 50ms, with starts 0, 50_000, 100_000.
    let completed = run_window(&[(1, 14, 0), (2, 14, 50_000), (3, 14, 100_000)], 50_000);
    assert_eq!(completed.len(), 3);
    assert!(
        completed.iter().all(|(_, c, _)| !c),
        "back-to-back TXs must not collide: {:?}",
        completed,
    );
}

// ---------------------------------------------------------------------------
// Property-based: arbitrary-size capture scenarios
// ---------------------------------------------------------------------------

proptest! {
    /// For an arbitrary set of overlapping equal-duration TXs, a TX `t` is
    /// the unique survivor iff `t.power >= max_other_power + threshold`.
    /// All other overlappers are collided.
    #[test]
    fn unique_survivor_iff_dominates_all_others(
        powers in prop::collection::vec(0i8..30i8, 2..6usize),
    ) {
        let threshold = ChannelConfig::lora_defaults().co_channel_rejection_db;
        let mut arrivals = Vec::with_capacity(powers.len());
        for (i, &p) in powers.iter().enumerate() {
            // Stagger starts in 100us increments inside a 50ms window so
            // every TX overlaps with every other.
            arrivals.push((i as u32 + 1, p, (i as u64) * 100));
        }
        let completed = run_window(&arrivals, 50_000);

        // Compute the expected survivor set: the set of TXs whose power
        // exceeds every other power by at least `threshold`.
        let mut expected_survivors: HashSet<u32> = HashSet::new();
        for (i, &pi) in powers.iter().enumerate() {
            let dominates_all_others = powers.iter().enumerate().all(|(j, &pj)| {
                i == j || (pi as f32 - pj as f32) >= threshold
            });
            if dominates_all_others {
                expected_survivors.insert(i as u32 + 1);
            }
        }
        // At most one TX can dominate all others.
        prop_assert!(expected_survivors.len() <= 1);

        let survivors: HashSet<u32> = completed
            .iter()
            .filter(|(_, c, _)| !c)
            .map(|(id, _, _)| id.0)
            .collect();

        prop_assert_eq!(survivors, expected_survivors);
    }

    /// For any arrangement of TXs on the same SF/freq, no completed TX is
    /// reported with both `collided` and a delivered receiver count > 0.
    /// We test this by asserting the scheduler's invariant
    /// `total_rx == (total_tx - total_collisions) * non_sender_count`.
    #[test]
    fn scheduler_metrics_partition_holds_for_random_powers(
        powers in prop::collection::vec(0i8..30i8, 2..5usize),
    ) {
        let n_senders = powers.len();
        let n_listeners = 2usize;
        let total_nodes = n_senders + n_listeners;

        let mut sched = Scheduler::new(500_000);
        for (i, &p) in powers.iter().enumerate() {
            sched.add_node(
                Box::new(OneShot::new(i as u32 + 1, tx_at_power(p))),
                Some((i as u64) * 100),
            );
        }
        for i in 0..n_listeners {
            sched.add_node(Box::new(Listener::new(100 + i as u32)), None);
        }
        sched.run();

        prop_assert_eq!(sched.metrics.total_tx, n_senders as u64);
        let delivered = sched.metrics.total_tx - sched.metrics.total_collisions;
        // Each delivered TX reaches `total_nodes - 1` non-sender receivers.
        prop_assert_eq!(
            sched.metrics.total_rx,
            delivered * (total_nodes as u64 - 1),
        );
        // Captures cannot exceed delivered transmissions.
        prop_assert!(sched.metrics.total_captures <= delivered);
    }
}

// ---------------------------------------------------------------------------
// Channel-level: a strict capture-threshold config makes more 3-way windows
// collide outright.
// ---------------------------------------------------------------------------

/// Sanity: a 3-way overlap where the strongest dominates by exactly the
/// LoRa threshold survives on the default channel, but the same scenario
/// with a stricter threshold has no survivor.
#[test]
fn strict_threshold_eliminates_marginal_dominators() {
    // Strong=20 dominates middle=14 and low=8 by exactly 6 dB / 12 dB.
    let arrivals: Vec<(u32, i8, SimTime)> = vec![(1, 20, 0), (2, 14, 1_000), (3, 8, 2_000)];

    let lora_completed = {
        let mut ch = Channel::new();
        for &(id, p, t) in &arrivals {
            ch.begin_transmission(NodeId(id), &tx_at_power(p), t);
        }
        ch.resolve_at(60_000);
        ch.drain_completed()
    };
    let strict_completed = {
        let mut ch = Channel::with_config(ChannelConfig {
            co_channel_rejection_db: 10.0,
            ..ChannelConfig::lora_defaults()
        });
        for &(id, p, t) in &arrivals {
            ch.begin_transmission(NodeId(id), &tx_at_power(p), t);
        }
        ch.resolve_at(60_000);
        ch.drain_completed()
    };

    let lora_survivors: Vec<_> = lora_completed.iter().filter(|(_, c, _, _)| !c).collect();
    let strict_survivors: Vec<_> = strict_completed.iter().filter(|(_, c, _, _)| !c).collect();

    assert_eq!(
        lora_survivors.len(),
        1,
        "LoRa default threshold=6 -> strongest survives"
    );
    assert_eq!(
        strict_survivors.len(),
        0,
        "strict threshold=10 -> 6 dB margin is not enough, all collide"
    );
}
