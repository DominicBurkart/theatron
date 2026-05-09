//! Integration tests covering undertested core components.
//!
//! These complement the existing unit tests and integration tests by targeting
//! gaps identified through coverage analysis: channel edge cases, scheduler
//! ordering invariants, metrics isolation, and time conversion edge cases.

use theatron::channel::Channel;
use theatron::metrics::MetricsCollector;
use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::{SimTime, ms_to_sim_time, sim_time_to_ms};
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tx(sf: u8, freq: u32, dur: u64, power: i8) -> Transmission {
    Transmission {
        payload: vec![0xAA],
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency: freq,
        duration_us: dur,
        tx_power_dbm: power,
    }
}

fn tx_with_payload(payload: Vec<u8>, sf: u8, freq: u32, dur: u64, power: i8) -> Transmission {
    Transmission {
        payload,
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency: freq,
        duration_us: dur,
        tx_power_dbm: power,
    }
}

struct TestNode {
    id: NodeId,
    pending_tx: Option<Transmission>,
    received: Vec<RxMetadata>,
    wake_schedule: Vec<SimTime>,
    wake_idx: usize,
}

impl TestNode {
    fn new(id: u32) -> Self {
        Self {
            id: NodeId(id),
            pending_tx: None,
            received: Vec::new(),
            wake_schedule: Vec::new(),
            wake_idx: 0,
        }
    }

    fn with_tx(mut self, t: Transmission) -> Self {
        self.pending_tx = Some(t);
        self
    }

    fn with_wakes(mut self, wakes: Vec<SimTime>) -> Self {
        self.wake_schedule = wakes;
        self
    }
}

impl NodeHandle for TestNode {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        self.received.push(frame);
        None
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        self.pending_tx.take()
    }

    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        if self.wake_idx < self.wake_schedule.len() {
            let t = self.wake_schedule[self.wake_idx];
            self.wake_idx += 1;
            Some(t)
        } else {
            None
        }
    }
}

/// Node that replies with a transmission upon receiving a frame.
struct EchoNode {
    id: NodeId,
    reply: Option<Transmission>,
    received: Vec<RxMetadata>,
}

impl EchoNode {
    fn new(id: u32, reply: Transmission) -> Self {
        Self {
            id: NodeId(id),
            reply: Some(reply),
            received: Vec::new(),
        }
    }
}

impl NodeHandle for EchoNode {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        self.received.push(frame);
        None
    }
    /// Replies exactly once, on the first poll after the first receive.
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        if !self.received.is_empty() {
            self.reply.take()
        } else {
            None
        }
    }
    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

struct CountingInterferer {
    tx: Transmission,
    remaining: usize,
    interval: u64,
}

impl InterferenceSource for CountingInterferer {
    fn observe(&mut self, _event: &ChannelEvent, _time: SimTime) {}
    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        if self.remaining > 0 {
            self.remaining -= 1;
            Some(self.tx.clone())
        } else {
            None
        }
    }
    fn next_poll_time(&self, current_time: SimTime) -> Option<SimTime> {
        if self.remaining > 0 {
            Some(current_time + self.interval)
        } else {
            None
        }
    }
}

// ===========================================================================
// Channel edge cases
// ===========================================================================

#[test]
fn channel_empty_resolve_returns_nothing() {
    let mut ch = Channel::new();
    let events = ch.resolve_at(1_000_000);
    assert!(events.is_empty());
}

#[test]
fn channel_empty_drain_returns_nothing() {
    let mut ch = Channel::new();
    assert!(ch.drain_completed().is_empty());
}

#[test]
fn channel_resolve_partial_only_finished() {
    let mut ch = Channel::new();
    // TX1 ends at 50_000, TX2 ends at 150_000
    ch.begin_transmission(NodeId(1), &tx(7, 868_100_000, 50_000, 14), 0);
    ch.begin_transmission(NodeId(2), &tx(8, 868_100_000, 150_000, 14), 0);

    let events = ch.resolve_at(60_000);
    // Only TX1 should have completed
    assert_eq!(events.len(), 1);
    let completed = ch.drain_completed();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].0, NodeId(1));
}

#[test]
fn channel_compute_rssi_and_snr_with_default_path_loss() {
    // Coupling: Channel::new() uses path_loss_db=100.0 and noise_floor_dbm=-117.0.
    // Expected values are derived from those defaults:
    //   rssi = tx_power_dbm - path_loss_db = 14 - 100 = -86.0
    //   snr  = rssi - noise_floor_dbm      = -86 - (-117) = 31.0
    let ch = Channel::new();
    let expected_rssi = 14.0_f32 - 100.0; // tx_power - path_loss_db
    let expected_snr = expected_rssi - (-117.0_f32); // rssi - noise_floor_dbm
    let rssi = ch.compute_rssi(14);
    assert!((rssi - expected_rssi).abs() < 0.001);
    let snr = ch.compute_snr(rssi);
    assert!((snr - expected_snr).abs() < 0.001);
}

#[test]
fn channel_compute_rssi_negative_power() {
    let ch = Channel::new();
    let rssi = ch.compute_rssi(-10);
    assert!((rssi - (-110.0)).abs() < 0.001);
}

#[test]
fn channel_resolve_filters_by_time() {
    let mut ch = Channel::new();
    // TX1 ends at 50_000, TX2 ends at 150_000
    ch.begin_transmission(NodeId(1), &tx(7, 868_100_000, 50_000, 14), 0);
    ch.begin_transmission(NodeId(2), &tx(8, 868_100_000, 150_000, 14), 0);
    ch.resolve_at(60_000);

    // drain_completed at this point only includes TX1 (only it was resolved)
    let completed_partial = ch.drain_completed();
    assert_eq!(completed_partial.len(), 1);
    assert_eq!(completed_partial[0].0, NodeId(1));

    // Resolve TX2 and confirm it drains correctly
    ch.resolve_at(200_000);
    let completed_rest = ch.drain_completed();
    assert_eq!(completed_rest.len(), 1);
    assert_eq!(completed_rest[0].0, NodeId(2));
}

#[test]
fn channel_late_strong_captures_earlier_weak() {
    // Weak signal starts first, then a strong signal arrives and captures it.
    // resolve_at uses end <= time (inclusive), so both TXs resolve at t=80_000:
    //   weak TX: start=0, dur=80_000 -> ends at 80_000 (included)
    //   strong TX: start=10_000, dur=60_000 -> ends at 70_000 (included)
    let mut ch = Channel::new();
    let weak = tx(7, 868_100_000, 80_000, 8);
    let strong = tx(7, 868_100_000, 60_000, 20);
    ch.begin_transmission(NodeId(1), &weak, 0);
    ch.begin_transmission(NodeId(2), &strong, 10_000);
    ch.resolve_at(80_000);
    let completed = ch.drain_completed();

    let strong_entry = completed
        .iter()
        .find(|(id, _, _, _)| *id == NodeId(2))
        .unwrap();
    let weak_entry = completed
        .iter()
        .find(|(id, _, _, _)| *id == NodeId(1))
        .unwrap();
    assert!(!strong_entry.1, "strong signal should not be collided");
    assert!(strong_entry.2, "strong signal should be captured");
    assert!(weak_entry.1, "weak signal should be collided");
}

#[test]
fn channel_begin_transmission_returns_started_event() {
    let mut ch = Channel::new();
    let event = ch.begin_transmission(NodeId(42), &tx(7, 868_100_000, 50_000, 14), 1_000);
    match event {
        ChannelEvent::TransmissionStarted {
            sender,
            sf,
            frequency,
            time,
        } => {
            assert_eq!(sender, NodeId(42));
            assert_eq!(sf, 7);
            assert_eq!(frequency, 868_100_000);
            assert_eq!(time, 1_000);
        }
        _ => panic!("expected TransmissionStarted"),
    }
}

#[test]
fn channel_resolve_returns_completed_events() {
    let mut ch = Channel::new();
    ch.begin_transmission(NodeId(1), &tx(7, 868_100_000, 50_000, 14), 0);
    let events = ch.resolve_at(50_000);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ChannelEvent::TransmissionCompleted {
            sender,
            collided,
            time,
        } => {
            assert_eq!(*sender, NodeId(1));
            assert!(!collided);
            assert_eq!(*time, 50_000);
        }
        _ => panic!("expected TransmissionCompleted"),
    }
}

#[test]
fn channel_multiple_resolve_cycles() {
    let mut ch = Channel::new();
    ch.begin_transmission(NodeId(1), &tx(7, 868_100_000, 50_000, 14), 0);
    ch.resolve_at(50_000);
    ch.drain_completed();

    ch.begin_transmission(NodeId(2), &tx(7, 868_100_000, 30_000, 14), 100_000);
    ch.resolve_at(130_000);
    let completed = ch.drain_completed();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].0, NodeId(2));
}

// ===========================================================================
// Channel proptest
// ===========================================================================

proptest! {
    #[test]
    fn capture_is_asymmetric(
        strong_power in 14i8..=22,
        weak_power in -10i8..=8,
    ) {
        prop_assume!(strong_power as f32 - weak_power as f32 >= 6.0);
        let mut ch = Channel::new();
        ch.begin_transmission(NodeId(1), &tx(7, 868_100_000, 50_000, strong_power), 0);
        ch.begin_transmission(NodeId(2), &tx(7, 868_100_000, 50_000, weak_power), 10_000);
        ch.resolve_at(60_000);
        let completed = ch.drain_completed();
        // Strong signal (NodeId(1)) should survive; weak signal (NodeId(2)) should not.
        let survivors: Vec<_> = completed.iter().filter(|(_, collided, _, _)| !collided).collect();
        prop_assert_eq!(survivors.len(), 1);
        // Verify the survivor is the strong sender by both NodeId and RSSI.
        prop_assert_eq!(survivors[0].0, NodeId(1));
        prop_assert_eq!(survivors[0].3.rssi, ch.compute_rssi(strong_power));
    }

    #[test]
    fn resolve_at_before_end_returns_nothing(start in 0u64..1_000_000, dur in 2u64..1_000_000) {
        let mut ch = Channel::new();
        ch.begin_transmission(NodeId(1), &tx(7, 868_100_000, dur, 14), start);
        // Resolve at the midpoint, which is strictly before the transmission ends.
        // dur >= 2 ensures dur/2 >= 1, so midpoint is always strictly less than start+dur.
        let midpoint = start + dur / 2;
        let events = ch.resolve_at(midpoint);
        prop_assert!(events.is_empty());
    }
}

// ===========================================================================
// Scheduler invariants
// ===========================================================================

#[test]
fn scheduler_current_time_progresses() {
    let mut sched = Scheduler::new(500_000);
    let node = TestNode::new(1).with_wakes(vec![100_000, 200_000, 300_000]);
    sched.add_node(Box::new(node), Some(0));
    sched.run();
    // current_time should be at least 300_000 (last scheduled wake)
    assert!(sched.current_time() >= 300_000);
}

#[test]
fn scheduler_events_beyond_end_time_are_skipped() {
    let mut sched = Scheduler::new(100_000);
    // Node schedules a wake at 200_000 which is beyond end_time
    let node = TestNode::new(1).with_wakes(vec![200_000]);
    sched.add_node(Box::new(node), Some(0));
    sched.run();
    assert!(sched.current_time() <= 100_000);
}

#[test]
fn scheduler_determinism() {
    fn run_scenario() -> (u64, u64, u64, u64) {
        let mut sched = Scheduler::new(500_000);
        let mut s1 = TestNode::new(1);
        s1.pending_tx = Some(tx(7, 868_100_000, 50_000, 14));
        let mut s2 = TestNode::new(2);
        s2.pending_tx = Some(tx(7, 868_100_000, 50_000, 14));
        sched.add_node(Box::new(s1), Some(0));
        sched.add_node(Box::new(s2), Some(10_000));
        sched.add_node(Box::new(TestNode::new(3)), None);
        sched.run();
        (
            sched.metrics.total_tx,
            sched.metrics.total_rx,
            sched.metrics.total_collisions,
            sched.current_time(),
        )
    }
    assert_eq!(run_scenario(), run_scenario());
}

#[test]
fn scheduler_multi_hop_reply_chain() {
    // A sends to B, B echoes back, A receives the reply
    let mut sched = Scheduler::new(500_000);
    let sender = TestNode::new(1).with_tx(tx_with_payload(vec![0x01], 7, 868_100_000, 30_000, 14));
    let echo = EchoNode::new(2, tx_with_payload(vec![0x02], 7, 868_100_000, 30_000, 14));
    sched.add_node(Box::new(sender), Some(0));
    sched.add_node(Box::new(echo), None);
    sched.run();

    assert_eq!(sched.metrics.total_tx, 2, "original + reply");
    assert_eq!(
        sched.metrics.total_rx, 2,
        "each node receives the other's TX"
    );
}

#[test]
fn scheduler_two_interferers_simultaneously() {
    let mut sched = Scheduler::new(300_000);
    sched.add_node(Box::new(TestNode::new(1)), None);

    let i1 = CountingInterferer {
        tx: tx(7, 868_100_000, 30_000, 14),
        remaining: 2,
        interval: 100_000,
    };
    let i2 = CountingInterferer {
        tx: tx(8, 868_300_000, 20_000, 10),
        remaining: 3,
        interval: 80_000,
    };
    sched.add_interferer(Box::new(i1), 0);
    sched.add_interferer(Box::new(i2), 0);
    sched.run();

    // Total airtime = 2*30_000 + 3*20_000 = 120_000
    assert_eq!(sched.metrics.total_airtime_us, 120_000);
}

#[test]
fn scheduler_interferer_and_node_different_sf_no_collision() {
    let mut sched = Scheduler::new(200_000);
    let mut node = TestNode::new(1);
    node.pending_tx = Some(tx(7, 868_100_000, 50_000, 14));
    sched.add_node(Box::new(node), Some(0));
    sched.add_node(Box::new(TestNode::new(2)), None);

    // Interferer on different SF
    let interferer = CountingInterferer {
        tx: tx(12, 868_100_000, 50_000, 14),
        remaining: 1,
        interval: 0,
    };
    sched.add_interferer(Box::new(interferer), 10_000);
    sched.run();

    assert_eq!(sched.metrics.total_collisions, 0);
    // Node 1 TX (SF7) -> delivered to Node 2 (1 RX)
    // Interferer TX (SF12) -> delivered to Node 1 and Node 2 (2 RX)
    // Total: 3 RX
    assert_eq!(
        sched.metrics.total_rx, 3,
        "node TX + interferer TX delivered on different SFs"
    );
}

#[test]
fn scheduler_no_nodes_no_events_finishes_immediately() {
    let mut sched = Scheduler::new(1_000_000);
    sched.run();
    assert_eq!(sched.current_time(), 0);
    assert_eq!(sched.metrics.total_tx, 0);
}

#[test]
fn scheduler_node_tx_count_per_node() {
    let mut sched = Scheduler::new(500_000);
    let mut n1 = TestNode::new(1);
    n1.pending_tx = Some(tx(7, 868_100_000, 30_000, 14));
    sched.add_node(Box::new(n1), Some(0));
    sched.add_node(Box::new(TestNode::new(2)), None);
    sched.run();

    assert_eq!(sched.metrics.node_tx_count(NodeId(1)), 1);
    assert_eq!(sched.metrics.node_tx_count(NodeId(2)), 0);
    assert_eq!(sched.metrics.node_rx_count(NodeId(2)), 1);
    assert_eq!(sched.metrics.node_rx_count(NodeId(1)), 0);
}

// ===========================================================================
// Scheduler proptest
// ===========================================================================

proptest! {
    #[test]
    fn scheduler_airtime_equals_sum_of_durations(
        dur1 in 10_000u64..100_000,
        dur2 in 10_000u64..100_000,
    ) {
        // The two TXs use different SFs (7 and 8), so they are orthogonal and
        // cannot collide regardless of timing overlap. Non-collision here is due
        // to different-SF isolation, not non-overlapping airtime windows.
        let mut sched = Scheduler::new(1_000_000);
        let mut n1 = TestNode::new(1);
        n1.pending_tx = Some(tx(7, 868_100_000, dur1, 14));
        let mut n2 = TestNode::new(2);
        n2.pending_tx = Some(tx(8, 868_100_000, dur2, 14));
        sched.add_node(Box::new(n1), Some(0));
        sched.add_node(Box::new(n2), Some(0));
        sched.run();
        prop_assert_eq!(sched.metrics.total_collisions, 0);
        prop_assert_eq!(sched.metrics.total_airtime_us, dur1 + dur2);
    }
}

// ===========================================================================
// Metrics isolation
// ===========================================================================

#[test]
fn metrics_per_node_counters_are_independent() {
    let mut m = MetricsCollector::new();
    m.record_tx(NodeId(1));
    m.record_tx(NodeId(1));
    m.record_tx(NodeId(2));
    m.record_rx(NodeId(3));
    m.record_rx(NodeId(3));
    m.record_rx(NodeId(3));

    assert_eq!(m.node_tx_count(NodeId(1)), 2);
    assert_eq!(m.node_tx_count(NodeId(2)), 1);
    assert_eq!(m.node_tx_count(NodeId(3)), 0);
    assert_eq!(m.node_rx_count(NodeId(1)), 0);
    assert_eq!(m.node_rx_count(NodeId(3)), 3);
    assert_eq!(m.total_tx, 3);
    assert_eq!(m.total_rx, 3);
}

#[test]
fn metrics_default_is_same_as_new() {
    let m1 = MetricsCollector::new();
    let m2 = MetricsCollector::default();
    assert_eq!(m1.total_tx, m2.total_tx);
    assert_eq!(m1.total_rx, m2.total_rx);
    assert_eq!(m1.total_collisions, m2.total_collisions);
    assert_eq!(m1.total_captures, m2.total_captures);
    assert_eq!(m1.total_airtime_us, m2.total_airtime_us);
}

// ===========================================================================
// Time conversion edge cases
// ===========================================================================

#[test]
fn sim_time_to_ms_truncates_sub_millisecond() {
    // 1_500 us = 1.5 ms, should truncate to 1
    assert_eq!(sim_time_to_ms(1_500), 1);
    assert_eq!(sim_time_to_ms(999), 0);
    assert_eq!(sim_time_to_ms(1_999), 1);
}

#[test]
fn ms_to_sim_time_max_u32() {
    let result = ms_to_sim_time(u32::MAX);
    assert_eq!(result, u32::MAX as u64 * 1_000);
}

proptest! {
    #[test]
    fn sim_time_to_ms_never_exceeds_input(us in 0u64..u64::MAX / 2) {
        let ms = sim_time_to_ms(us);
        prop_assert!(ms * 1_000 <= us);
    }
}
