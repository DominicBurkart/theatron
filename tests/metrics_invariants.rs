//! Property-based and targeted tests for MetricsCollector invariants and the
//! Protocol::update scheduling contract.
//!
//! Gaps addressed:
//! * No proptest verified that `total_tx` equals the sum of all per-node TX
//!   counts for arbitrary node-id/count sequences.
//! * No proptest verified the same invariant for RX.
//! * `record_collision` and `record_capture` were never exercised together in
//!   the same collector to prove they are independent counters.
//! * The `Protocol::update` contract – returning `Some(t)` reschedules the
//!   node; returning `None` stops it – had no property test.

use theatron::metrics::MetricsCollector;
use theatron::traits::Protocol;
use theatron::types::{NodeId, RxMetadata, Transmission};

use proptest::prelude::*;

// ===========================================================================
// MetricsCollector aggregate invariants
// ===========================================================================

proptest! {
    /// total_tx must equal the sum of every per-node TX count, for any
    /// sequence of (node_id, count) pairs with up to 16 distinct nodes.
    #[test]
    fn total_tx_equals_sum_of_per_node_tx(
        counts in prop::collection::vec((0u32..=15, 0u32..=50), 0..=16)
    ) {
        let mut m = MetricsCollector::new();
        for (id, n) in &counts {
            for _ in 0..*n {
                m.record_tx(NodeId(*id));
            }
        }
        // Sum per-node counts (deduplicated by node id)
        let mut per_node_sum: u64 = 0;
        // Collect unique node ids seen
        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for (id, _) in &counts {
            seen.insert(*id);
        }
        for id in seen {
            per_node_sum += m.node_tx_count(NodeId(id));
        }
        prop_assert_eq!(m.total_tx, per_node_sum,
            "total_tx ({}) != sum of per-node tx counts ({})",
            m.total_tx, per_node_sum);
    }

    /// total_rx must equal the sum of every per-node RX count.
    #[test]
    fn total_rx_equals_sum_of_per_node_rx(
        counts in prop::collection::vec((0u32..=15, 0u32..=50), 0..=16)
    ) {
        let mut m = MetricsCollector::new();
        for (id, n) in &counts {
            for _ in 0..*n {
                m.record_rx(NodeId(*id));
            }
        }
        let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for (id, _) in &counts {
            seen.insert(*id);
        }
        let mut per_node_sum: u64 = 0;
        for id in seen {
            per_node_sum += m.node_rx_count(NodeId(id));
        }
        prop_assert_eq!(m.total_rx, per_node_sum,
            "total_rx ({}) != sum of per-node rx counts ({})",
            m.total_rx, per_node_sum);
    }

    /// Airtime accumulation is commutative: order of record_airtime calls must
    /// not affect the final total.
    #[test]
    fn airtime_accumulation_is_order_independent(
        durations in prop::collection::vec(0u64..=1_000_000u64, 0..=20)
    ) {
        let mut m1 = MetricsCollector::new();
        let mut m2 = MetricsCollector::new();
        // Forward order
        for &d in &durations {
            m1.record_airtime(d);
        }
        // Reverse order
        for &d in durations.iter().rev() {
            m2.record_airtime(d);
        }
        prop_assert_eq!(m1.total_airtime_us, m2.total_airtime_us);
        prop_assert_eq!(m1.total_airtime_us, durations.iter().sum::<u64>());
    }
}

// ---------------------------------------------------------------------------
// Collision and capture independence
// ---------------------------------------------------------------------------

/// record_collision and record_capture must increment independent counters;
/// neither call should affect the other's total.
#[test]
fn collision_and_capture_are_independent_counters() {
    let mut m = MetricsCollector::new();
    m.record_collision();
    m.record_collision();
    m.record_collision();
    m.record_capture();
    m.record_capture();
    assert_eq!(m.total_collisions, 3);
    assert_eq!(m.total_captures, 2);
    // TX and RX should remain untouched
    assert_eq!(m.total_tx, 0);
    assert_eq!(m.total_rx, 0);
}

/// Interleaving collision and capture records should not cause interference.
#[test]
fn interleaved_collision_capture_records() {
    let mut m = MetricsCollector::new();
    for _ in 0..5 {
        m.record_collision();
        m.record_capture();
    }
    assert_eq!(m.total_collisions, 5);
    assert_eq!(m.total_captures, 5);
}

// ===========================================================================
// Protocol::update scheduling contract
// ===========================================================================

/// A Protocol whose `update` returns `Some(wake)` exactly `reschedule_count`
/// times before returning `None`. Tracks how many times `update` was called.
struct CountingProtocol;

struct CountingState {
    remaining: u32,
    update_calls: u32,
}

impl Protocol for CountingProtocol {
    type Config = u32; // how many reschedules to perform
    type State = CountingState;
    type Metrics = ();

    fn init(&self, config: u32) -> (CountingState, Option<u64>) {
        let wake = if config > 0 { Some(0) } else { None };
        (CountingState { remaining: config, update_calls: 0 }, wake)
    }

    fn on_receive(
        &self,
        _state: &mut CountingState,
        _frame: RxMetadata,
        _time: u64,
    ) -> Option<u64> {
        None
    }

    fn poll_transmit(
        &self,
        _state: &mut CountingState,
        _time: u64,
    ) -> Option<Transmission> {
        None
    }

    fn update(&self, state: &mut CountingState, _time: u64) -> Option<u64> {
        state.update_calls += 1;
        if state.remaining > 0 {
            state.remaining -= 1;
            Some(1_000) // reschedule 1 ms later
        } else {
            None
        }
    }

    fn metrics(&self, _state: &CountingState) {}
}

/// When `update` always returns `None`, the protocol never requests further
/// wakeups after the first call.
#[test]
fn protocol_update_none_stops_scheduling() {
    let proto = CountingProtocol;
    let (mut state, _) = proto.init(0);
    let first = proto.update(&mut state, 0);
    assert!(first.is_none(), "update should return None when remaining == 0");
    assert_eq!(state.update_calls, 1);
    // A second call should still return None (idempotent once exhausted)
    let second = proto.update(&mut state, 1_000);
    assert!(second.is_none());
    assert_eq!(state.update_calls, 2);
}

/// When `update` returns `Some(t)` it signals the scheduler to call again at
/// time `t`; after `n` such returns it must return `None`.
#[test]
fn protocol_update_reschedules_exactly_n_times() {
    let proto = CountingProtocol;
    let reschedules = 4u32;
    let (mut state, initial_wake) = proto.init(reschedules);
    assert!(initial_wake.is_some(), "init should wake the node when reschedules > 0");

    let mut actual_reschedules = 0u32;
    let mut t = 0u64;
    loop {
        match proto.update(&mut state, t) {
            Some(next_t) => {
                actual_reschedules += 1;
                t = next_t;
            }
            None => break,
        }
    }
    assert_eq!(
        actual_reschedules, reschedules,
        "update should return Some exactly {} times", reschedules
    );
    assert_eq!(state.update_calls, reschedules + 1, // N Some + 1 None
        "update should have been called {} times total", reschedules + 1
    );
}

proptest! {
    /// For any reschedule count n in 0..=20, update returns Some exactly n
    /// times then None, and state.update_calls == n + 1.
    #[test]
    fn protocol_update_call_count_matches_reschedule_count(n in 0u32..=20) {
        let proto = CountingProtocol;
        let (mut state, _) = proto.init(n);
        let mut actual = 0u32;
        let mut t = 0u64;
        loop {
            match proto.update(&mut state, t) {
                Some(next_t) => { actual += 1; t = next_t; }
                None => break,
            }
        }
        prop_assert_eq!(actual, n);
        prop_assert_eq!(state.update_calls, n + 1);
    }
}
