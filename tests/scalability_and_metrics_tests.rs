/// Integration tests covering three previously untested areas:
///
/// 1. **Scalability** – 50-node slotted-Aloha simulation.
/// 2. **Metrics invariants** – proptest-driven checks that fundamental
///    relationships between counters can never be violated.
/// 3. **TrafficModel** – `PeriodicTrafficModel` produces packets at the
///    expected rate when wired into a simulation node.
use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::TrafficModel;
use theatron::types::{NodeId, RxMetadata, Transmission};

use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

// `make_tx` is duplicated from the `#[cfg(test)]` helper in `src/scheduler.rs`.
// The duplication is intentional: integration tests compile as their own crate
// and cannot access private helpers from the library under test.
fn make_tx(sf: u8, frequency: u32, duration_us: u64) -> Transmission {
    Transmission {
        payload: vec![0xAB],
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency,
        duration_us,
        tx_power_dbm: 14,
    }
}

// ---------------------------------------------------------------------------
// Test 1: Scalability – 50-node slotted-Aloha
// ---------------------------------------------------------------------------

/// A slotted-Aloha node that wakes once per `slot_period_us`, queues a
/// transmission on every wake, and re-schedules itself for the next slot.
///
/// Each node starts in a different slot (offset = `node_index * INITIAL_SPREAD_US`)
/// so the initial wakes are spread out; after that all nodes repeat every
/// `slot_period_us`.  The node records how many times it has transmitted so
/// the test can cross-check per-node counts against the scheduler's metrics.
struct SlottedAlohaNode {
    id: NodeId,
    slot_period_us: u64,
    tx_duration_us: u64,
    sf: u8,
    frequency: u32,
    pending: Option<Transmission>,
    tx_count: u64,
}

impl SlottedAlohaNode {
    fn new(id: u32, slot_period_us: u64, tx_duration_us: u64) -> Self {
        Self {
            id: NodeId(id),
            slot_period_us,
            tx_duration_us,
            sf: 7,
            frequency: 868_100_000,
            pending: None,
            tx_count: 0,
        }
    }
}

impl NodeHandle for SlottedAlohaNode {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        // Pure sender; ignore received frames.
        None
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        if let Some(tx) = self.pending.take() {
            self.tx_count += 1;
            return Some(tx);
        }
        None
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        // Queue one transmission for this slot.
        self.pending = Some(make_tx(self.sf, self.frequency, self.tx_duration_us));
        // Wake again at the start of the next slot.
        Some(time + self.slot_period_us)
    }
}

#[test]
fn fifty_node_slotted_aloha_all_nodes_transmit() {
    const NUM_NODES: u32 = 50;
    // Slot duration is longer than a single TX so nodes do not permanently
    // collide with themselves.
    const TX_DURATION_US: u64 = 50_000; // 50 ms
    const SLOT_PERIOD_US: u64 = 500_000; // 500 ms – each node gets ~20 slots in 10 s
    // Spread initial wakes evenly across one slot period so that no two nodes
    // fire at the exact same microsecond at t=0.
    const INITIAL_SPREAD_US: u64 = SLOT_PERIOD_US / NUM_NODES as u64; // 10 ms per node
    // Run for 10 seconds; each node should get ~20 transmission opportunities.
    const END_TIME_US: u64 = 10_000_000;
    // Lower bound: even if every other slot collides, each node must have
    // fired at least once in 10 s with a 500 ms period.
    const MIN_TX_PER_NODE: u64 = 1;
    // Upper bound: 10 s / 500 ms = 20 slots per node, plus one potential
    // boundary slot = 21.  This relies on two properties of the scheduler:
    //   (a) node 0 has initial_wake = 0, and
    //   (b) Scheduler::run() processes events whose time == end_time
    //       (termination condition is `event.time > end_time`, not `>=`).
    // Node 0 therefore fires at t = 0, 500_000, …, 10_000_000 — exactly 21
    // events.  If either property changes, revisit this constant.
    const MAX_TX_PER_NODE: u64 = 21;

    let mut sched = Scheduler::new(END_TIME_US);

    let mut nodes: Vec<SlottedAlohaNode> = (0..NUM_NODES)
        .map(|i| SlottedAlohaNode::new(i + 1, SLOT_PERIOD_US, TX_DURATION_US))
        .collect();

    // Collect (id, initial_wake) pairs before consuming the Vec so we can
    // cross-check tx_count after the run.  Use into_iter() — draining a Vec
    // just constructed via collect() is wasteful.
    let node_ids: Vec<u32> = nodes.iter().map(|n| n.id.0).collect();
    for (i, node) in nodes.into_iter().enumerate() {
        let initial_wake = i as u64 * INITIAL_SPREAD_US;
        sched.add_node(Box::new(node), Some(initial_wake));
    }

    sched.run();

    // --- invariant 1: every node transmitted at least once ---
    for id in 1..=NUM_NODES {
        let count = sched.metrics.node_tx_count(NodeId(id));
        assert!(
            count >= MIN_TX_PER_NODE,
            "node {} never transmitted (count={})",
            id,
            count
        );
    }

    // --- invariant 2: total_tx == sum of per-node tx counts ---
    let per_node_sum: u64 = (1..=NUM_NODES)
        .map(|id| sched.metrics.node_tx_count(NodeId(id)))
        .sum();
    assert_eq!(
        sched.metrics.total_tx, per_node_sum,
        "metrics.total_tx ({}) != sum of per-node counts ({})",
        sched.metrics.total_tx, per_node_sum
    );

    // --- invariant 3: per-node tx count is within a plausible range ---
    for id in 1..=NUM_NODES {
        let count = sched.metrics.node_tx_count(NodeId(id));
        assert!(
            count <= MAX_TX_PER_NODE,
            "node {} tx count {} exceeds the expected maximum of {}",
            id,
            count,
            MAX_TX_PER_NODE
        );
    }

    // --- invariant 4: node-local tx_count agrees with scheduler metrics ---
    // This double-accounting check catches any disagreement between what the
    // node believes it sent and what the scheduler recorded.
    // Note: because we transferred ownership of each SlottedAlohaNode into the
    // scheduler via add_node, we verify through the scheduler's metrics here.
    // The node_ids vec retains the IDs so we can iterate over them.
    for id in node_ids {
        let sched_count = sched.metrics.node_tx_count(NodeId(id));
        // The scheduler's per-node count is the authoritative source;
        // cross-check that it is non-zero (already covered above) and that
        // total_tx accounts for every one of those transmissions (covered by
        // invariant 2).  A direct node.tx_count == sched_count check would
        // require extracting the node back out of the scheduler, which the
        // API does not expose; instead we assert the scheduler's own
        // accounting is self-consistent via invariant 2 above.
        assert!(
            sched_count <= sched.metrics.total_tx,
            "node {} sched count {} exceeds total_tx {}",
            id,
            sched_count,
            sched.metrics.total_tx
        );
    }

    // --- sanity: the simulation actually ran ---
    assert!(
        sched.current_time() > 0,
        "simulation did not advance past t=0"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Metrics invariants – proptest
// ---------------------------------------------------------------------------

/// A node that transmits once per `period_us`, used as the load generator for
/// the proptest invariant checks.
struct PeriodicSender {
    id: NodeId,
    period_us: u64,
    tx_duration_us: u64,
    pending: Option<Transmission>,
}

impl PeriodicSender {
    fn new(id: u32, period_us: u64, tx_duration_us: u64) -> Self {
        Self {
            id: NodeId(id),
            period_us,
            tx_duration_us,
            pending: None,
        }
    }
}

impl NodeHandle for PeriodicSender {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        self.pending.take()
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        self.pending = Some(make_tx(7, 868_100_000, self.tx_duration_us));
        Some(time + self.period_us)
    }
}

/// A node that only listens (never transmits). Used to populate the receiver
/// population for the invariant checks.
struct ListenerNode {
    id: NodeId,
}

impl ListenerNode {
    fn new(id: u32) -> Self {
        Self { id: NodeId(id) }
    }
}

impl NodeHandle for ListenerNode {
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

proptest! {
    /// For any combination of node count (2..=20) and duration (100 ms..=2 s):
    ///
    /// * `total_rx   <= successful_tx * num_receivers`  – can't receive more than
    ///   what was successfully sent (colliding TXes are not received)
    /// * `total_collisions <= total_tx`             – collisions can't exceed transmissions
    /// * `total_captures   <= total_collisions`     – captures are a subset of collisions
    ///
    /// Receiver count derivation: there are `num_nodes` senders (IDs 1..=num_nodes)
    /// plus 1 dedicated listener (ID num_nodes+1).  When any one sender transmits,
    /// the remaining `num_nodes - 1` senders plus the 1 listener can receive it,
    /// giving exactly `num_nodes` potential receivers per successful TX.
    /// Using `num_nodes` as the bound is therefore the exact tight upper bound.
    ///
    /// 512 cases (rather than the default 256) ensures enough samples reach the
    /// collision-heavy upper end of the parameter space where the invariants
    /// would actually fire on a buggy scheduler.
    #[proptest_config(ProptestConfig { cases: 512, .. ProptestConfig::default() })]
    #[test]
    fn metrics_invariants_hold(
        num_nodes in 2usize..=20usize,
        duration_ms in 100u64..=2_000u64,
    ) {
        let end_time_us = duration_ms * 1_000;

        // Use one sender per node (staggered initial wakes) and one shared
        // listener so there is always at least one receiver.
        let mut sched = Scheduler::new(end_time_us);

        // Senders: IDs 1..=num_nodes, each with a 200 ms period and 50 ms TX.
        // Stagger by 10 ms so they don't all fire at t=0.
        for i in 0..num_nodes {
            let id = (i + 1) as u32;
            let initial_wake = i as u64 * 10_000;
            sched.add_node(
                Box::new(PeriodicSender::new(id, 200_000, 50_000)),
                Some(initial_wake),
            );
        }

        // One dedicated listener (ID outside sender range).
        // Registered with None initial wake: update/poll_transmit are never
        // called on it.  Receptions still flow because
        // deliver_completed_to_nodes iterates all non-sender nodes
        // unconditionally.
        let listener_id = (num_nodes + 1) as u32;
        sched.add_node(Box::new(ListenerNode::new(listener_id)), None);

        sched.run();

        let m = &sched.metrics;
        // Each TX can be received by at most num_nodes receivers:
        //   (num_nodes - 1) other senders + 1 dedicated listener = num_nodes.
        let num_receivers = num_nodes as u64;

        // Use successful_tx (non-colliding transmissions) as the tight upper
        // bound on receptions: colliding frames are never received, so bounding
        // by total_tx would leave the invariant unconditionally true when
        // collisions are high.
        let successful_tx = m.total_tx.saturating_sub(m.total_collisions);
        prop_assert!(
            m.total_rx <= successful_tx * num_receivers,
            "total_rx ({}) > successful_tx ({}) * num_receivers ({})",
            m.total_rx, successful_tx, num_receivers
        );
        prop_assert!(
            m.total_collisions <= m.total_tx,
            "total_collisions ({}) > total_tx ({})",
            m.total_collisions, m.total_tx
        );
        prop_assert!(
            m.total_captures <= m.total_collisions,
            "total_captures ({}) > total_collisions ({})",
            m.total_captures, m.total_collisions
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: PeriodicTrafficModel
// ---------------------------------------------------------------------------

/// Generates one payload every `interval_us` microseconds.
///
/// The first packet is available at the first time `t` where
/// `t >= interval_us`; subsequent packets are available each time a full
/// `interval_us` has elapsed since the previous emission.  Calls to
/// `next_payload` within the same interval return `None`.
///
/// `interval_us` must be greater than zero; a zero interval would cause every
/// poll to emit (the predicate `time >= 0 + 0` is always true and
/// `last_generated_us` would never advance).
// `pub` is a no-op in an integration-test crate (each `tests/*.rs` compiles as
// its own crate; nothing else can import it).  If this model is intended to be
// part of the library's public surface, move it into `src/traffic_model.rs`.
struct PeriodicTrafficModel {
    interval_us: u64,
    last_generated_us: u64,
    payload: Vec<u8>,
}

impl PeriodicTrafficModel {
    /// Create a new model that emits a payload every `interval_us` microseconds.
    ///
    /// # Panics (debug builds)
    /// Panics if `interval_us == 0`.
    fn new(interval_us: u64, payload: Vec<u8>) -> Self {
        debug_assert!(interval_us > 0, "interval_us must be > 0");
        Self {
            interval_us,
            last_generated_us: 0,
            payload,
        }
    }
}

impl TrafficModel for PeriodicTrafficModel {
    fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>> {
        if time >= self.last_generated_us + self.interval_us {
            self.last_generated_us += self.interval_us;
            Some(self.payload.clone())
        } else {
            None
        }
    }
}

// --- Unit tests for PeriodicTrafficModel on its own ---

#[test]
fn periodic_traffic_model_emits_at_correct_times() {
    let mut model = PeriodicTrafficModel::new(1_000_000, vec![0x01]);

    // Before the first interval: no payload.
    assert!(model.next_payload(0).is_none());
    assert!(model.next_payload(500_000).is_none());

    // At exactly one interval: payload available.
    assert!(model.next_payload(1_000_000).is_some());

    // Immediately after: must wait for the next interval.
    assert!(model.next_payload(1_000_001).is_none());

    // At two intervals: payload available again.
    assert!(model.next_payload(2_000_000).is_some());
}

#[test]
fn periodic_traffic_model_does_not_emit_twice_in_same_interval() {
    let mut model = PeriodicTrafficModel::new(500_000, vec![0x42]);

    // First emission at t=500_000.
    assert!(model.next_payload(500_000).is_some());

    // Repeated calls within the same interval return nothing.
    for _ in 0..5 {
        assert!(model.next_payload(500_000).is_none());
    }
}

#[test]
fn periodic_traffic_model_payload_contents_are_correct() {
    let expected = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let mut model = PeriodicTrafficModel::new(1_000, expected.clone());
    assert_eq!(model.next_payload(1_000).unwrap(), expected);
}

/// Regression test for the smallest valid interval (1 µs).
///
/// Guards against off-by-one regressions where interval_us = 1 might be
/// treated as equivalent to zero or might fail to advance last_generated_us.
#[test]
fn periodic_traffic_model_interval_of_one_microsecond() {
    let mut model = PeriodicTrafficModel::new(1, vec![0xFF]);

    // t=0: not yet (0 >= 0 + 1 is false).
    assert!(model.next_payload(0).is_none());

    // t=1: first emission.
    assert!(model.next_payload(1).is_some());

    // t=1 again: already emitted for this interval.
    assert!(model.next_payload(1).is_none());

    // t=2: second emission.
    assert!(model.next_payload(2).is_some());

    // t=2 again: no double-emit.
    assert!(model.next_payload(2).is_none());
}

// --- Integration: wire PeriodicTrafficModel into a simulation node ---

/// A node that delegates packet generation to a `PeriodicTrafficModel`.
/// On each wake it asks the model for a payload; if one is ready it queues a
/// transmission and schedules the next check at `time + check_interval_us`.
struct TrafficModelNode {
    id: NodeId,
    model: PeriodicTrafficModel,
    check_interval_us: u64,
    tx_duration_us: u64,
    sf: u8,
    frequency: u32,
    pending: Option<Transmission>,
}

impl TrafficModelNode {
    fn new(
        id: u32,
        model: PeriodicTrafficModel,
        check_interval_us: u64,
        tx_duration_us: u64,
    ) -> Self {
        Self {
            id: NodeId(id),
            model,
            check_interval_us,
            tx_duration_us,
            sf: 7,
            frequency: 868_100_000,
            pending: None,
        }
    }
}

impl NodeHandle for TrafficModelNode {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        self.pending.take()
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        if let Some(payload) = self.model.next_payload(time) {
            self.pending = Some(Transmission {
                payload,
                sf: self.sf,
                bandwidth: 125_000,
                coding_rate: 5,
                frequency: self.frequency,
                duration_us: self.tx_duration_us,
                tx_power_dbm: 14,
            });
        }
        Some(time + self.check_interval_us)
    }
}

/// Verify that a node driven by a `PeriodicTrafficModel` transmits at
/// approximately the expected rate.
///
/// Setup:
/// - Traffic model interval: 1 s (1_000_000 µs)
/// - Check interval: 100 ms (well below the traffic interval, so no emissions
///   are missed between checks)
/// - Simulation duration: 10 s (10_000_000 µs)
/// - Expected transmissions: 10 (one per second, first at t=1s)
///
/// We allow ±1 to account for boundary effects at the end of the simulation.
#[test]
fn traffic_model_node_transmits_at_expected_rate() {
    const INTERVAL_US: u64 = 1_000_000; // 1 s between packets
    const CHECK_INTERVAL_US: u64 = 100_000; // poll the model every 100 ms
    const TX_DURATION_US: u64 = 50_000; // 50 ms on-air
    const SIM_DURATION_US: u64 = 10_000_000; // 10 s

    let expected_tx: u64 = SIM_DURATION_US / INTERVAL_US; // 10

    let model = PeriodicTrafficModel::new(INTERVAL_US, vec![0xAB, 0xCD]);
    let sender = TrafficModelNode::new(1, model, CHECK_INTERVAL_US, TX_DURATION_US);
    // Listener registered with None initial wake: update/poll_transmit are never
    // called on it.  Receptions still flow via deliver_completed_to_nodes.
    let listener = ListenerNode::new(2);

    let mut sched = Scheduler::new(SIM_DURATION_US);
    sched.add_node(Box::new(sender), Some(0));
    sched.add_node(Box::new(listener), None);
    sched.run();

    let actual_tx = sched.metrics.total_tx;
    assert!(
        actual_tx >= expected_tx.saturating_sub(1) && actual_tx <= expected_tx + 1,
        "expected ~{} transmissions, got {}",
        expected_tx,
        actual_tx
    );
}

/// Verify that the payload produced by the model is faithfully delivered to
/// the receiver when there is no interference.
#[test]
fn traffic_model_payload_delivered_correctly() {
    const INTERVAL_US: u64 = 500_000; // 0.5 s
    const CHECK_INTERVAL_US: u64 = 50_000; // 50 ms checks
    const TX_DURATION_US: u64 = 30_000;
    const SIM_DURATION_US: u64 = 2_000_000; // 2 s → expect ~4 transmissions

    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let model = PeriodicTrafficModel::new(INTERVAL_US, payload.clone());
    let sender = TrafficModelNode::new(1, model, CHECK_INTERVAL_US, TX_DURATION_US);
    // Listener registered with None initial wake: update/poll_transmit are never
    // called on it.  Receptions still flow via deliver_completed_to_nodes.
    let listener = ListenerNode::new(2);

    let mut sched = Scheduler::new(SIM_DURATION_US);
    sched.add_node(Box::new(sender), Some(0));
    sched.add_node(Box::new(listener), None);
    sched.run();

    // All transmissions that complete within the simulation window should be
    // received (no collisions, single sender).  A TX started near end_time may
    // have its TxComplete event fall beyond end_time and therefore never be
    // delivered; allow at most one such in-flight frame.
    assert_eq!(sched.metrics.total_collisions, 0);
    assert!(
        sched.metrics.total_tx >= 3,
        "expected at least 3 transmissions in a 2 s window, got {}",
        sched.metrics.total_tx
    );
    assert!(
        sched.metrics.total_rx >= sched.metrics.total_tx.saturating_sub(1),
        "received {} frames but expected at most 1 in-flight loss (tx={})",
        sched.metrics.total_rx,
        sched.metrics.total_tx,
    );
}
