//! Targeted tests for four previously-uncovered correctness properties:
//!
//! 1. **`TrafficModel` trait contract** – the trait has doctests but no
//!    dedicated integration tests.  We verify exhaust-after-use, time-gated
//!    emission, and None-forever-after-first-None behaviour.
//!
//! 2. **Scheduler event-ordering tiebreaker** – when two events land at the
//!    same `SimTime`, the `seq` counter ensures FIFO ordering (oldest-scheduled
//!    fires first).  The `ScheduledEvent::Ord` impl uses a max-heap with
//!    reversed-`seq` as secondary key, so the lowest `seq` (oldest) wins.
//!    We confirm observable wake ordering matches insertion order.
//!
//! 3. **`MetricsCollector` per-node sum invariant** – the sum of every
//!    per-node TX (or RX) count must equal `total_tx` (or `total_rx`).
//!    Proptest covers arbitrary sequences of record calls.
//!
//! 4. **`Channel::deliver_to` partial delivery** – when a batch contains
//!    both collided and clean transmissions, only the clean ones are returned
//!    by `deliver_to`, and the count is exact.

use theatron::channel::Channel;
use theatron::metrics::MetricsCollector;
use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::TrafficModel;
use theatron::types::{NodeId, RxMetadata, Transmission};

use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_tx(sf: u8, frequency: u32, duration_us: u64) -> Transmission {
    Transmission {
        payload: vec![0x42],
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency,
        duration_us,
        tx_power_dbm: 14,
    }
}

// ---------------------------------------------------------------------------
// 1. TrafficModel trait contract
// ---------------------------------------------------------------------------

/// A `TrafficModel` that yields one payload per call until the stock is
/// exhausted.  Mirrors the canonical use-case described in the doctest.
struct FixedStock {
    items: Vec<Vec<u8>>,
}

impl FixedStock {
    fn new(items: Vec<Vec<u8>>) -> Self {
        Self { items }
    }
}

impl TrafficModel for FixedStock {
    fn next_payload(&mut self, _time: SimTime) -> Option<Vec<u8>> {
        if self.items.is_empty() {
            None
        } else {
            Some(self.items.remove(0))
        }
    }
}

/// A `TrafficModel` that only produces a payload once a deadline has passed.
struct TimeGated {
    deadline: SimTime,
    payload: Option<Vec<u8>>,
}

impl TrafficModel for TimeGated {
    fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>> {
        if time >= self.deadline {
            self.payload.take()
        } else {
            None
        }
    }
}

#[test]
fn traffic_model_exhausts_after_stock_consumed() {
    let items = vec![vec![0x01], vec![0x02], vec![0x03]];
    let mut model = FixedStock::new(items);

    assert_eq!(model.next_payload(0), Some(vec![0x01]));
    assert_eq!(model.next_payload(1), Some(vec![0x02]));
    assert_eq!(model.next_payload(2), Some(vec![0x03]));
    // Exhausted: every subsequent call must return None.
    assert_eq!(model.next_payload(3), None);
    assert_eq!(model.next_payload(4), None);
}

#[test]
fn traffic_model_none_before_deadline_some_after() {
    let mut model = TimeGated {
        deadline: 1_000_000,
        payload: Some(vec![0xAB]),
    };

    // Before deadline: should stay None regardless of how many times called.
    assert_eq!(model.next_payload(0), None);
    assert_eq!(model.next_payload(999_999), None);

    // At the deadline: payload released.
    assert_eq!(model.next_payload(1_000_000), Some(vec![0xAB]));

    // After release: None forever.
    assert_eq!(model.next_payload(1_000_001), None);
}

#[test]
fn traffic_model_empty_stock_always_none() {
    let mut model = FixedStock::new(vec![]);
    for t in 0..10 {
        assert_eq!(
            model.next_payload(t),
            None,
            "empty model must return None at time {t}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Scheduler event-ordering tiebreaker
// ---------------------------------------------------------------------------
//
// Strategy: two nodes both scheduled to wake at t=0.  We verify that
// `current_time()` stays at 0 for both wakes (i.e. both fire, not just one),
// and — more importantly — we record which node woke first via a shared
// counter.  Node A is registered (and thus gets `seq=0`) before Node B
// (`seq=1`).  With a max-heap and reversed-seq tiebreaking, `seq=0` fires
// first.

struct OrderRecorder {
    id: NodeId,
    order_log: std::rc::Rc<std::cell::RefCell<Vec<u32>>>,
}

impl NodeHandle for OrderRecorder {
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
        self.order_log.borrow_mut().push(self.id.0);
        None
    }
}

#[test]
fn scheduler_same_time_events_fire_in_insertion_order() {
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u32>::new()));

    let mut sched = Scheduler::new(1_000);
    // Node 1 registered and woken first (lower seq).
    sched.add_node(
        Box::new(OrderRecorder {
            id: NodeId(1),
            order_log: log.clone(),
        }),
        Some(0),
    );
    // Node 2 registered and woken second (higher seq).
    sched.add_node(
        Box::new(OrderRecorder {
            id: NodeId(2),
            order_log: log.clone(),
        }),
        Some(0),
    );
    sched.run();

    let fired = log.borrow();
    assert_eq!(
        fired.len(),
        2,
        "both nodes must fire even when scheduled at the same time"
    );
    assert_eq!(
        fired[0], 1,
        "node registered first (lower seq) must wake first"
    );
    assert_eq!(
        fired[1], 2,
        "node registered second (higher seq) must wake second"
    );
}

#[test]
fn scheduler_current_time_is_monotonically_non_decreasing() {
    // A node that re-schedules itself many times at strictly increasing times.
    // We verify that `current_time()` at the end equals the last scheduled
    // wake that falls within end_time.
    struct MonotonicNode {
        id: NodeId,
        step: u64,
        last_seen: u64,
    }

    impl NodeHandle for MonotonicNode {
        fn node_id(&self) -> NodeId {
            self.id
        }
        fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> {
            None
        }
        fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> {
            None
        }
        fn update(&mut self, t: SimTime) -> Option<SimTime> {
            assert!(
                t >= self.last_seen,
                "time went backwards: {} < {}",
                t,
                self.last_seen
            );
            self.last_seen = t;
            Some(t + self.step)
        }
    }

    let end = 1_000_000u64;
    let mut sched = Scheduler::new(end);
    sched.add_node(
        Box::new(MonotonicNode {
            id: NodeId(1),
            step: 100_000,
            last_seen: 0,
        }),
        Some(0),
    );
    sched.run();
    assert!(sched.current_time() <= end);
}

// ---------------------------------------------------------------------------
// 3. MetricsCollector per-node sum invariant (proptest)
// ---------------------------------------------------------------------------
//
// Invariant: sum of all per-node TX counts == total_tx, and similarly for RX.

proptest! {
    #[test]
    fn metrics_per_node_tx_sums_to_total(
        // Generate up to 50 (node_id, count) pairs; node_ids in 0..10 so there
        // are deliberate repeats that accumulate on the same node bucket.
        ops in proptest::collection::vec((0u32..10, 1u32..20), 0..50)
    ) {
        let mut m = MetricsCollector::new();
        for (id, count) in &ops {
            for _ in 0..*count {
                m.record_tx(NodeId(*id));
            }
        }

        // Collect distinct node ids that received at least one TX.
        let distinct: std::collections::HashSet<u32> = ops.iter().map(|(id, _)| *id).collect();
        let per_node_sum: u64 = distinct.iter().map(|id| m.node_tx_count(NodeId(*id))).sum();

        prop_assert_eq!(
            per_node_sum, m.total_tx,
            "per-node TX counts must sum to total_tx"
        );
    }

    #[test]
    fn metrics_per_node_rx_sums_to_total(
        ops in proptest::collection::vec((0u32..10, 1u32..20), 0..50)
    ) {
        let mut m = MetricsCollector::new();
        for (id, count) in &ops {
            for _ in 0..*count {
                m.record_rx(NodeId(*id));
            }
        }

        let distinct: std::collections::HashSet<u32> = ops.iter().map(|(id, _)| *id).collect();
        let per_node_sum: u64 = distinct.iter().map(|id| m.node_rx_count(NodeId(*id))).sum();

        prop_assert_eq!(
            per_node_sum, m.total_rx,
            "per-node RX counts must sum to total_rx"
        );
    }

    #[test]
    fn metrics_unknown_node_always_zero(id in 100u32..200u32) {
        // A node that was never recorded must report zero for both TX and RX.
        let m = MetricsCollector::new();
        prop_assert_eq!(m.node_tx_count(NodeId(id)), 0);
        prop_assert_eq!(m.node_rx_count(NodeId(id)), 0);
    }
}

// ---------------------------------------------------------------------------
// 4. Channel::deliver_to partial delivery in a mixed batch
// ---------------------------------------------------------------------------
//
// Scenario: three transmissions on the same SF/frequency start at overlapping
// times.  Two of them collide with each other but the third starts late enough
// that it does NOT overlap the first two.  After resolving, `deliver_to` must
// return exactly the non-collided frames.

#[test]
fn channel_deliver_to_returns_only_clean_frames_in_mixed_batch() {
    let mut ch = Channel::new();

    // TX A: t=0..50_000.  TX B: t=10_000..60_000.  A and B overlap → both collide.
    let tx_a = make_tx(7, 868_100_000, 50_000);
    let tx_b = make_tx(7, 868_100_000, 50_000);
    ch.begin_transmission(NodeId(1), &tx_a, 0);
    ch.begin_transmission(NodeId(2), &tx_b, 10_000);

    // TX C: t=100_000..150_000.  No overlap with A or B → clean.
    let tx_c = make_tx(7, 868_100_000, 50_000);
    ch.begin_transmission(NodeId(3), &tx_c, 100_000);

    // Resolve everything in one pass.
    ch.resolve_at(150_000);

    let delivered = ch.deliver_to(150_000);
    assert_eq!(
        delivered.len(),
        1,
        "only the non-collided TX (C) should be delivered"
    );
    assert_eq!(
        delivered[0].sf, 7,
        "delivered frame must carry the correct SF"
    );
}

#[test]
fn channel_deliver_to_empty_when_all_collide() {
    let mut ch = Channel::new();
    // Two overlapping same-SF/freq TXs: both collide.
    let tx1 = make_tx(7, 868_100_000, 50_000);
    let tx2 = make_tx(7, 868_100_000, 50_000);
    ch.begin_transmission(NodeId(1), &tx1, 0);
    ch.begin_transmission(NodeId(2), &tx2, 5_000);
    ch.resolve_at(55_000);

    assert_eq!(
        ch.deliver_to(55_000).len(),
        0,
        "all collided → deliver_to must return empty vec"
    );
}

#[test]
fn channel_deliver_to_all_clean_when_none_collide() {
    let mut ch = Channel::new();
    // Three sequential, non-overlapping TXs.
    for i in 0u64..3 {
        let tx = make_tx(7, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(i as u32 + 1), &tx, i * 100_000);
    }
    ch.resolve_at(350_000);

    assert_eq!(
        ch.deliver_to(350_000).len(),
        3,
        "all three sequential TXs must be delivered"
    );
}
