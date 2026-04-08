//! Cross-cutting simulation invariants validated as property-based and
//! integration tests.  These invariants hold for *any* honest scheduler
//! run, regardless of node count, transmission pattern, or interference:
//!
//!  1. `total_rx <= total_tx`        – you can only receive what was sent
//!  2. `total_collisions <= total_tx` – a collision requires a transmission
//!  3. `total_captures <= total_tx`   – a capture requires a transmission
//!  4. airtime is recorded for every TX (node + interferer)
//!  5. scheduler never advances past `end_time`
//!  6. a scheduler with `end_time = 0` runs without panicking and stays at t=0
//!  7. events scheduled at the same wall-clock time are processed in
//!     insertion order (seq ascending), preserving determinism

use proptest::prelude::*;
use theatron::{
    metrics::MetricsCollector,
    scheduler::{NodeHandle, Scheduler},
    time::{SimTime, ms_to_sim_time},
    traits::InterferenceSource,
    types::{ChannelEvent, NodeId, RxMetadata, Transmission},
};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn tx(sf: u8, freq: u32, duration_us: u64, power: i8) -> Transmission {
    Transmission {
        payload: vec![0xAB],
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency: freq,
        duration_us,
        tx_power_dbm: power,
    }
}

const SF: u8 = 7;
const FREQ: u32 = 868_100_000;
const DUR: u64 = 50_000;

/// A node that fires one transmission at its first wake and then goes silent.
struct OnceSender {
    id: NodeId,
    tx: Option<Transmission>,
}

impl OnceSender {
    fn new(id: u32, t: Transmission) -> Self {
        Self { id: NodeId(id), tx: Some(t) }
    }
}

impl NodeHandle for OnceSender {
    fn node_id(&self) -> NodeId { self.id }
    fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> { None }
    fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> { self.tx.take() }
    fn update(&mut self, _t: SimTime) -> Option<SimTime> { None }
}

/// A purely passive listener.
struct Listener { id: NodeId }

impl NodeHandle for Listener {
    fn node_id(&self) -> NodeId { self.id }
    fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> { None }
    fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> { None }
    fn update(&mut self, _t: SimTime) -> Option<SimTime> { None }
}

/// A periodic sender that fires `count` transmissions spaced `gap_us` apart.
struct PeriodicSender {
    id: NodeId,
    remaining: usize,
    gap_us: u64,
    tx_template: Transmission,
}

impl PeriodicSender {
    fn new(id: u32, count: usize, gap_us: u64, t: Transmission) -> Self {
        Self { id: NodeId(id), remaining: count, gap_us, tx_template: t }
    }
}

impl NodeHandle for PeriodicSender {
    fn node_id(&self) -> NodeId { self.id }
    fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> { None }
    fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> {
        if self.remaining > 0 {
            Some(self.tx_template.clone())
        } else {
            None
        }
    }
    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        if self.remaining == 0 { return None; }
        self.remaining -= 1;
        if self.remaining > 0 {
            Some(time + self.tx_template.duration_us + self.gap_us)
        } else {
            None
        }
    }
}

/// An interferer that injects exactly `count` transmissions.
struct CountingInterferer {
    tx: Transmission,
    period: u64,
    remaining: usize,
}

impl InterferenceSource for CountingInterferer {
    fn observe(&mut self, _e: &ChannelEvent, _t: SimTime) {}
    fn poll_inject(&mut self, _t: SimTime) -> Option<Transmission> {
        if self.remaining > 0 {
            self.remaining -= 1;
            Some(self.tx.clone())
        } else {
            None
        }
    }
    fn next_poll_time(&self, now: SimTime) -> Option<SimTime> {
        if self.remaining > 0 { Some(now + self.period) } else { None }
    }
}

// ---------------------------------------------------------------------------
// Invariant 1 + 2 + 3: rx <= tx, collisions <= tx, captures <= tx
// ---------------------------------------------------------------------------

/// Runs `n` senders (each firing once) against `m` listeners and asserts
/// the fundamental metric ordering invariants.
proptest! {
    #[test]
    fn metric_ordering_invariants(
        n_senders in 1usize..6,
        n_listeners in 1usize..6,
        with_collision in proptest::bool::ANY,
    ) {
        let end = ms_to_sim_time(500);
        let mut sched = Scheduler::new(end);

        // When with_collision=true all senders fire on the same SF+freq at
        // the same time, guaranteeing collisions.
        let start_time: SimTime = if with_collision { 0 } else { 0 };
        for i in 0..n_senders {
            // Stagger by 1 µs when we *don't* want collisions so frames fit.
            let wake = if with_collision { 0 } else { i as u64 * (DUR + 10_000) };
            sched.add_node(
                Box::new(OnceSender::new(i as u32, tx(SF, FREQ, DUR, 14))),
                Some(wake),
            );
        }
        let _ = start_time; // suppress lint
        for j in 0..n_listeners {
            sched.add_node(
                Box::new(Listener { id: NodeId((n_senders + j) as u32) }),
                None,
            );
        }
        sched.run();

        let m = &sched.metrics;
        prop_assert!(m.total_rx <= m.total_tx,
            "total_rx ({}) must not exceed total_tx ({})", m.total_rx, m.total_tx);
        prop_assert!(m.total_collisions <= m.total_tx,
            "collisions ({}) must not exceed total_tx ({})", m.total_collisions, m.total_tx);
        prop_assert!(m.total_captures <= m.total_tx,
            "captures ({}) must not exceed total_tx ({})", m.total_captures, m.total_tx);
    }
}

// ---------------------------------------------------------------------------
// Invariant 4: airtime conservation
// ---------------------------------------------------------------------------

/// Every node TX contributes exactly its `duration_us` to total_airtime_us.
#[test]
fn airtime_equals_sum_of_tx_durations() {
    // Three senders with distinct durations, no overlap.
    let durations = [30_000u64, 45_000, 60_000];
    let end = ms_to_sim_time(2_000);
    let mut sched = Scheduler::new(end);

    let mut t = 0u64;
    for (i, &dur) in durations.iter().enumerate() {
        sched.add_node(
            Box::new(OnceSender::new(i as u32, tx(SF, FREQ, dur, 14))),
            Some(t),
        );
        t += dur + 10_000;
    }
    sched.add_node(Box::new(Listener { id: NodeId(99) }), None);
    sched.run();

    let expected: u64 = durations.iter().sum();
    assert_eq!(sched.metrics.total_airtime_us, expected,
        "airtime must equal sum of all TX durations");
}

/// Interferer airtime is also accumulated.
#[test]
fn interferer_airtime_is_accumulated() {
    let end = ms_to_sim_time(500);
    let mut sched = Scheduler::new(end);
    sched.add_node(Box::new(Listener { id: NodeId(0) }), None);
    let per_tx = 40_000u64;
    let count = 3usize;
    sched.add_interferer(
        Box::new(CountingInterferer {
            tx: tx(SF, FREQ, per_tx, 14),
            period: per_tx + 50_000,
            remaining: count,
        }),
        0,
    );
    sched.run();

    assert_eq!(sched.metrics.total_airtime_us, per_tx * count as u64,
        "each interferer injection must be reflected in total_airtime_us");
    // Interferer TXs must NOT increment total_tx (which is node-only)
    assert_eq!(sched.metrics.total_tx, 0);
}

// ---------------------------------------------------------------------------
// Invariant 5: scheduler never advances past end_time
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn scheduler_never_exceeds_end_time(end_ms in 1u32..5_000u32) {
        let end = ms_to_sim_time(end_ms);
        let mut sched = Scheduler::new(end);
        sched.add_node(
            Box::new(PeriodicSender::new(1, 100, 1_000, tx(SF, FREQ, DUR, 14))),
            Some(0),
        );
        sched.run();
        prop_assert!(sched.current_time() <= end,
            "current_time {} > end_time {}", sched.current_time(), end);
    }
}

// ---------------------------------------------------------------------------
// Invariant 6: zero-duration simulation
// ---------------------------------------------------------------------------

/// A scheduler with end_time=0 must not panic and must stay at t=0.
#[test]
fn zero_end_time_scheduler_is_safe() {
    let mut sched = Scheduler::new(0);
    sched.add_node(
        Box::new(OnceSender::new(1, tx(SF, FREQ, DUR, 14))),
        Some(0),
    );
    sched.run();
    // Nothing beyond t=0 is allowed; the node wake at t=0 is exactly at the
    // boundary so its TX complete event at t=50_000 is dropped.
    assert_eq!(sched.current_time(), 0);
    // total_tx may be 0 or 1 depending on whether the t=0 wake is processed;
    // the key property is that it did not panic.
}

// ---------------------------------------------------------------------------
// Invariant 7: same-time event ordering is deterministic (seq tiebreaker)
// ---------------------------------------------------------------------------

/// Two senders wake at exactly the same simulated time.  Running the
/// simulation twice must produce identical metrics (determinism).
#[test]
fn same_time_events_are_deterministic() {
    fn run_once() -> (u64, u64, u64) {
        let end = ms_to_sim_time(200);
        let mut sched = Scheduler::new(end);
        // Both nodes wake at t=0 — same-time tiebreaker via seq.
        sched.add_node(Box::new(OnceSender::new(1, tx(SF, FREQ, DUR, 14))), Some(0));
        sched.add_node(Box::new(OnceSender::new(2, tx(SF, FREQ, DUR, 20))), Some(0));
        sched.add_node(Box::new(Listener { id: NodeId(3) }), None);
        sched.run();
        (sched.metrics.total_tx, sched.metrics.total_rx, sched.metrics.total_collisions)
    }
    assert_eq!(run_once(), run_once(), "same-time scheduling must be deterministic");
}

// ---------------------------------------------------------------------------
// Multi-node per-node counts sum to global totals
// ---------------------------------------------------------------------------

/// The sum of per-node TX counts must equal the global TX counter.
#[test]
fn per_node_tx_sums_to_global_total() {
    let nodes: Vec<u32> = vec![1, 2, 3];
    let mut m = MetricsCollector::new();
    let counts = [3u64, 1, 5];
    for (&id, &c) in nodes.iter().zip(counts.iter()) {
        for _ in 0..c {
            m.record_tx(NodeId(id));
        }
    }
    let per_node_sum: u64 = nodes.iter().map(|&id| m.node_tx_count(NodeId(id))).sum();
    assert_eq!(per_node_sum, m.total_tx,
        "per-node TX counts must sum to global total_tx");
}

/// Same for RX.
#[test]
fn per_node_rx_sums_to_global_total() {
    let nodes: Vec<u32> = vec![10, 20, 30, 40];
    let mut m = MetricsCollector::new();
    let counts = [2u64, 0, 7, 3];
    for (&id, &c) in nodes.iter().zip(counts.iter()) {
        for _ in 0..c {
            m.record_rx(NodeId(id));
        }
    }
    let per_node_sum: u64 = nodes.iter().map(|&id| m.node_rx_count(NodeId(id))).sum();
    assert_eq!(per_node_sum, m.total_rx,
        "per-node RX counts must sum to global total_rx");
}

/// capture and collision counters are independent of each other.
#[test]
fn capture_and_collision_counters_are_independent() {
    let mut m = MetricsCollector::new();
    m.record_collision();
    m.record_collision();
    m.record_capture();
    assert_eq!(m.total_collisions, 2);
    assert_eq!(m.total_captures, 1);
    // Resetting by creating a new collector should reset both.
    let fresh = MetricsCollector::new();
    assert_eq!(fresh.total_collisions, 0);
    assert_eq!(fresh.total_captures, 0);
}

// ---------------------------------------------------------------------------
// Channel: deliver_to only includes frames resolved in earlier windows
// ---------------------------------------------------------------------------

/// `deliver_to` must not surface frames whose TX ended *after* the query
/// time, even if they are in the completed list from a prior resolve pass.
#[test]
fn deliver_to_respects_time_window() {
    use theatron::channel::Channel;
    use theatron::types::NodeId;

    let mut ch = Channel::new();

    // TX 1: ends at t=50_000
    ch.begin_transmission(NodeId(1), &tx(SF, FREQ, 50_000, 14), 0);
    // TX 2: ends at t=110_000
    ch.begin_transmission(NodeId(2), &tx(SF + 1, FREQ, 100_000, 14), 10_000);

    // Resolve both
    ch.resolve_at(110_000);

    // Querying at t=50_000 should only surface TX 1
    let early = ch.deliver_to(50_000);
    assert_eq!(early.len(), 1);
    assert_eq!(early[0].sf, SF);

    // Querying at t=110_000 should surface both
    let late = ch.deliver_to(110_000);
    assert_eq!(late.len(), 2);
}

// ---------------------------------------------------------------------------
// Channel: three-way with two strong signals — compound capture+collide
// ---------------------------------------------------------------------------

/// When two strong senders (power >= threshold above a weak one) both
/// collide with each other, neither is a clean capture winner.
/// Both strong senders must be marked collided.
#[test]
fn two_strong_vs_one_weak_neither_strong_captures() {
    use theatron::channel::Channel;
    use theatron::types::NodeId;

    // Default threshold = 6 dB.  Both strong senders are at 20 dBm;
    // they are equal power and therefore mutually collide.
    // The weak sender (14 dBm) is 6 dB below each strong one → it is
    // captured by both (its `captured` flag may be set but it is still
    // collided by the first strong tx it overlaps).
    let mut ch = Channel::new();
    ch.begin_transmission(NodeId(1), &tx(SF, FREQ, 50_000, 20), 0);
    ch.begin_transmission(NodeId(2), &tx(SF, FREQ, 50_000, 20), 5_000);
    ch.begin_transmission(NodeId(3), &tx(SF, FREQ, 50_000, 14), 10_000);
    ch.resolve_at(60_000);
    let completed = ch.drain_completed();

    // No clean delivery: all three collide.
    assert_eq!(completed.len(), 3);
    for (id, collided, _captured, _) in &completed {
        assert!(
            *collided,
            "NodeId({}) should be collided when two equally-strong senders overlap",
            id.0
        );
    }
}

// ---------------------------------------------------------------------------
// Time: sub-millisecond truncation is documented behaviour
// ---------------------------------------------------------------------------

#[test]
fn sub_millisecond_sim_time_truncates_to_zero() {
    use theatron::time::sim_time_to_ms;
    // 999 µs < 1 ms → floors to 0
    assert_eq!(sim_time_to_ms(999), 0, "999 µs must floor to 0 ms");
    assert_eq!(sim_time_to_ms(1_000), 1, "exactly 1 ms");
    assert_eq!(sim_time_to_ms(1_999), 1, "1999 µs still floors to 1 ms");
}

#[test]
fn ms_to_sim_time_u32_max_does_not_overflow() {
    use theatron::time::ms_to_sim_time;
    // u32::MAX = 4_294_967_295 ms — should not overflow a u64 result.
    let result = ms_to_sim_time(u32::MAX);
    assert_eq!(result, u32::MAX as u64 * 1_000);
}
