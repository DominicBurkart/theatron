//! Tests for two underspecified scheduler/channel invariants:
//!
//! 1. **Capture survivor is order-invariant.** When N transmissions overlap on
//!    the same SF/frequency, the unique frame that survives (delivered, not
//!    collided) is determined solely by the power profile, not by the order in
//!    which transmissions were registered with the channel. Tested as a
//!    proptest over all permutations of (begin_transmission) call ordering.
//!
//! 2. **Scheduler boundary at `end_time` is inclusive.** `Scheduler::run`
//!    breaks when `event.time > end_time`, so an event scheduled at exactly
//!    `end_time` must fire. This boundary is exercised by no existing test.
//!
//! These are pragmatic gap-fillers: they pin down behaviour that the
//! implementation already exhibits but that no test currently asserts.

use proptest::prelude::*;

use theatron::channel::Channel;
use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::types::{NodeId, RxMetadata, Transmission};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tx(payload: Vec<u8>, power: i8) -> Transmission {
    Transmission {
        payload,
        sf: 7,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency: 868_100_000,
        duration_us: 50_000,
        tx_power_dbm: power,
    }
}

/// A node that fires exactly one transmission on its first `update`/`poll_transmit`.
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
    fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> {
        None
    }
    fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> {
        self.pending.take()
    }
    fn update(&mut self, _t: SimTime) -> Option<SimTime> {
        None
    }
}

/// Records every wake time it observes via `update`, then stops.
struct WakeRecorder {
    id: NodeId,
    seen: std::rc::Rc<std::cell::RefCell<Vec<SimTime>>>,
}

impl NodeHandle for WakeRecorder {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> {
        None
    }
    fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> {
        None
    }
    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        self.seen.borrow_mut().push(time);
        None
    }
}

// ===========================================================================
// 1. Capture survivor is order-invariant
// ===========================================================================

/// Run three overlapping same-SF/freq transmissions through the channel in a
/// caller-supplied order; return the IDs of the survivors (non-collided).
fn run_three_way(order: [(u32, i8); 3]) -> Vec<u32> {
    let mut ch = Channel::new();
    // Stagger by 1 us so all three overlap (50_000 us duration ≫ 2 us).
    for (i, (id, power)) in order.iter().enumerate() {
        ch.begin_transmission(NodeId(*id), &tx(vec![*id as u8], *power), i as u64);
    }
    ch.resolve_at(60_000);
    ch.drain_completed()
        .into_iter()
        .filter(|(_, collided, _, _)| !*collided)
        .map(|(id, _, _, _)| id.0)
        .collect()
}

#[test]
fn three_way_strongest_wins_regardless_of_order() {
    // Powers 20 / 14 / 8 (deltas 6 / 6 / 12 — strongest dominates both others).
    // Across all 6 permutations the strongest (id=1, power=20) must be the
    // unique survivor. This pins down that capture is a function of power
    // alone, independent of `begin_transmission` call order.
    let powers = [(1u32, 20i8), (2, 14), (3, 8)];
    let mut perms = Vec::new();
    for i in 0..3 {
        for j in 0..3 {
            for k in 0..3 {
                if i != j && j != k && i != k {
                    perms.push([powers[i], powers[j], powers[k]]);
                }
            }
        }
    }
    assert_eq!(perms.len(), 6);

    for perm in perms {
        let survivors = run_three_way(perm);
        assert_eq!(
            survivors,
            vec![1],
            "strongest (id=1, 20 dBm) must be the unique survivor; perm={:?}",
            perm,
        );
    }
}

proptest! {
    /// For two overlapping same-SF/freq transmissions, the unique survivor
    /// (when one exists) is the higher-power sender — independent of
    /// `begin_transmission` order.
    #[test]
    fn two_way_capture_order_invariant(
        p_strong in 14i8..=22,
        p_weak in -10i8..=8,
        strong_first in any::<bool>(),
    ) {
        prop_assume!(p_strong as f32 - p_weak as f32 >= 6.0);
        let mut ch = Channel::new();
        let strong = tx(vec![0x01], p_strong);
        let weak = tx(vec![0x02], p_weak);
        if strong_first {
            ch.begin_transmission(NodeId(1), &strong, 0);
            ch.begin_transmission(NodeId(2), &weak, 10_000);
        } else {
            ch.begin_transmission(NodeId(2), &weak, 0);
            ch.begin_transmission(NodeId(1), &strong, 10_000);
        }
        ch.resolve_at(60_000);
        let survivors: Vec<u32> = ch
            .drain_completed()
            .into_iter()
            .filter(|(_, collided, _, _)| !*collided)
            .map(|(id, _, _, _)| id.0)
            .collect();
        prop_assert_eq!(survivors, vec![1]);
    }

    /// The collided/captured flag pair never reports a frame as both
    /// "delivered (not collided) AND the loser of capture". Specifically:
    /// for any same-SF/freq overlap, at most one transmission has
    /// `collided == false`.
    #[test]
    fn at_most_one_survivor_per_collision_group(
        p1 in 0i8..=22,
        p2 in 0i8..=22,
        p3 in 0i8..=22,
    ) {
        let mut ch = Channel::new();
        ch.begin_transmission(NodeId(1), &tx(vec![1], p1), 0);
        ch.begin_transmission(NodeId(2), &tx(vec![2], p2), 1);
        ch.begin_transmission(NodeId(3), &tx(vec![3], p3), 2);
        ch.resolve_at(60_000);
        let completed = ch.drain_completed();
        let survivors = completed.iter().filter(|(_, c, _, _)| !*c).count();
        prop_assert!(survivors <= 1);
    }
}

// ===========================================================================
// 2. Scheduler `end_time` boundary is inclusive
// ===========================================================================

#[test]
fn event_at_exact_end_time_fires() {
    // The scheduler stops on `event.time > end_time`, so an event scheduled
    // at exactly end_time must still fire. Without this assertion, a future
    // change to `>=` would silently drop boundary events.
    let end = 100_000u64;
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let recorder = WakeRecorder {
        id: NodeId(1),
        seen: std::rc::Rc::clone(&seen),
    };
    let mut sched = Scheduler::new(end);
    sched.add_node(Box::new(recorder), Some(end));
    sched.run();

    assert_eq!(
        *seen.borrow(),
        vec![end],
        "wake scheduled at exactly end_time must fire (got {:?})",
        seen.borrow()
    );
}

/// Pure receiver: never transmits, never wakes.
struct PureRx {
    id: NodeId,
}

impl NodeHandle for PureRx {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> {
        None
    }
    fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> {
        None
    }
    fn update(&mut self, _t: SimTime) -> Option<SimTime> {
        None
    }
}

#[test]
fn tx_completing_at_end_time_is_delivered() {
    // A TX whose completion lands exactly at end_time should still be
    // delivered to non-sender nodes. This is a sharper boundary check than
    // a wake-only event because TxComplete drives `deliver_completed_to_nodes`.
    let end = 50_000u64; // tx.duration_us == end → completes at exactly end
    let mut sched = Scheduler::new(end);
    sched.add_node(Box::new(OneShot::new(1, tx(vec![0xAB], 14))), Some(0));
    sched.add_node(Box::new(PureRx { id: NodeId(2) }), None);
    sched.run();

    assert_eq!(sched.metrics.total_tx, 1);
    assert_eq!(
        sched.metrics.total_rx, 1,
        "TX completing at exactly end_time must still deliver",
    );
}

// ===========================================================================
// 3. Total airtime is permutation-invariant under node registration order
// ===========================================================================

proptest! {
    #[test]
    fn airtime_invariant_under_registration_order(
        d1 in 10_000u64..80_000,
        d2 in 10_000u64..80_000,
        d3 in 10_000u64..80_000,
        permute in 0usize..6,
    ) {
        // Three nodes with different durations on different SFs → no collisions.
        // Total airtime must equal d1+d2+d3 for every registration permutation.
        let nodes: Vec<(u32, u8, u64)> = vec![
            (1, 7, d1),
            (2, 8, d2),
            (3, 9, d3),
        ];
        let perms: [[usize; 3]; 6] = [
            [0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0],
        ];
        let order = perms[permute];

        let mut sched = Scheduler::new(2_000_000);
        for &i in &order {
            let (id, sf, dur) = nodes[i];
            let mut t = tx(vec![id as u8], 14);
            t.sf = sf;
            t.duration_us = dur;
            sched.add_node(Box::new(OneShot::new(id, t)), Some(0));
        }
        sched.run();

        prop_assert_eq!(sched.metrics.total_airtime_us, d1 + d2 + d3);
        prop_assert_eq!(sched.metrics.total_tx, 3);
        prop_assert_eq!(sched.metrics.total_collisions, 0);
    }
}
