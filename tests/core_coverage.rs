//! Tests targeting undertested core components:
//! - Scheduler event ordering (ScheduledEvent Ord impl)
//! - Channel edge cases (empty resolve, idempotent drain, RSSI/SNR)
//! - Metrics per-node isolation
//! - Multi-node scheduler interleaving
//! - Proptest invariants for scheduler and channel

use theatron::channel::Channel;
use theatron::metrics::MetricsCollector;
use theatron::scheduler::{EventKind, NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::types::{NodeId, RxMetadata, Transmission};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_tx(sf: u8, frequency: u32, duration_us: u64, tx_power_dbm: i8) -> Transmission {
    Transmission {
        payload: vec![0xAA],
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency,
        duration_us,
        tx_power_dbm,
    }
}

fn default_tx() -> Transmission {
    make_tx(7, 868_100_000, 50_000, 14)
}

/// A node that transmits once per wake (not on receive-triggered polls).
/// Tracks how many times it was woken.
struct CountingNode {
    id: NodeId,
    interval_us: u64,
    wake_count: u32,
    tx_template: Option<Transmission>,
    ready_to_tx: bool,
    received: Vec<RxMetadata>,
}

impl CountingNode {
    fn new(id: u32, interval_us: u64, tx: Option<Transmission>) -> Self {
        Self {
            id: NodeId(id),
            interval_us,
            wake_count: 0,
            tx_template: tx,
            ready_to_tx: false,
            received: Vec::new(),
        }
    }
}

impl NodeHandle for CountingNode {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        self.received.push(frame);
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        if self.ready_to_tx {
            self.ready_to_tx = false;
            self.tx_template.clone()
        } else {
            None
        }
    }
    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        self.wake_count += 1;
        self.ready_to_tx = true;
        Some(time + self.interval_us)
    }
}

/// A node that does nothing -- never wakes, never transmits.
struct InertNode {
    id: NodeId,
}

impl NodeHandle for InertNode {
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

/// A node that transmits exactly once on its first wake.
struct OneShotNode {
    id: NodeId,
    tx: Option<Transmission>,
    fired: bool,
}

impl OneShotNode {
    fn new(id: u32, tx: Transmission) -> Self {
        Self {
            id: NodeId(id),
            tx: Some(tx),
            fired: false,
        }
    }
}

impl NodeHandle for OneShotNode {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> {
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        if self.fired {
            self.tx.take()
        } else {
            None
        }
    }
    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        self.fired = true;
        None // only one wake
    }
}

// ===========================================================================
// Channel edge-case tests
// ===========================================================================

#[test]
fn resolve_before_any_tx_ends_returns_empty() {
    let mut ch = Channel::new();
    let tx = default_tx(); // duration 50_000
    ch.begin_transmission(NodeId(1), &tx, 0);
    let events = ch.resolve_at(25_000);
    assert!(events.is_empty(), "no TX should complete before its end time");
}

#[test]
fn resolve_on_empty_channel_returns_empty() {
    let mut ch = Channel::new();
    let events = ch.resolve_at(100_000);
    assert!(events.is_empty());
}

#[test]
fn deliver_to_on_empty_channel_returns_empty() {
    let ch = Channel::new();
    let delivered = ch.deliver_to(100_000);
    assert!(delivered.is_empty());
}

#[test]
fn drain_completed_is_idempotent() {
    let mut ch = Channel::new();
    let tx = default_tx();
    ch.begin_transmission(NodeId(1), &tx, 0);
    ch.resolve_at(50_000);

    let first = ch.drain_completed();
    assert_eq!(first.len(), 1);

    let second = ch.drain_completed();
    assert!(second.is_empty(), "drain_completed must empty the list");
}

#[test]
fn deliver_to_does_not_drain() {
    let mut ch = Channel::new();
    let tx = default_tx();
    ch.begin_transmission(NodeId(1), &tx, 0);
    ch.resolve_at(50_000);

    let first = ch.deliver_to(50_000);
    assert_eq!(first.len(), 1);

    // deliver_to is non-destructive; calling again should yield the same result
    let second = ch.deliver_to(50_000);
    assert_eq!(second.len(), 1);
}

#[test]
fn compute_rssi_reflects_tx_power() {
    let ch = Channel::new();
    let rssi_low = ch.compute_rssi(0);
    let rssi_high = ch.compute_rssi(20);
    assert!(
        rssi_high > rssi_low,
        "higher TX power must yield higher RSSI"
    );
    // With default path_loss_db=100, rssi = tx_power - 100
    assert!((rssi_low - (-100.0_f32)).abs() < 0.001);
    assert!((rssi_high - (-80.0_f32)).abs() < 0.001);
}

#[test]
fn compute_snr_reflects_rssi() {
    let ch = Channel::new();
    // With default noise_floor_dbm=-117, snr = rssi - (-117) = rssi + 117
    let snr = ch.compute_snr(-86.0);
    assert!((snr - 31.0_f32).abs() < 0.001);
}

#[test]
fn collided_tx_not_in_deliver_to() {
    let mut ch = Channel::new();
    let tx1 = make_tx(7, 868_100_000, 50_000, 14);
    let tx2 = make_tx(7, 868_100_000, 50_000, 14);
    ch.begin_transmission(NodeId(1), &tx1, 0);
    ch.begin_transmission(NodeId(2), &tx2, 10_000);
    ch.resolve_at(60_000);

    let delivered = ch.deliver_to(60_000);
    assert!(delivered.is_empty(), "collided frames must not be delivered");

    // But drain_completed still returns them
    let completed = ch.drain_completed();
    assert_eq!(completed.len(), 2);
    assert!(completed.iter().all(|(_, collided, _, _)| *collided));
}

#[test]
fn partial_resolve_leaves_active_tx() {
    let mut ch = Channel::new();
    let short = make_tx(7, 868_100_000, 30_000, 14);
    let long = make_tx(7, 868_300_000, 80_000, 14); // different freq, no collision
    ch.begin_transmission(NodeId(1), &short, 0);
    ch.begin_transmission(NodeId(2), &long, 0);

    // Resolve at 30_000: only the short TX is done
    let events = ch.resolve_at(30_000);
    assert_eq!(events.len(), 1);

    // Resolve again at 80_000: now the long TX is done
    let events = ch.resolve_at(80_000);
    assert_eq!(events.len(), 1);
}

#[test]
fn begin_transmission_returns_started_event() {
    use theatron::types::ChannelEvent;
    let mut ch = Channel::new();
    let tx = default_tx();
    let event = ch.begin_transmission(NodeId(42), &tx, 1000);
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
            assert_eq!(time, 1000);
        }
        _ => panic!("expected TransmissionStarted"),
    }
}

// ===========================================================================
// Metrics per-node isolation tests
// ===========================================================================

#[test]
fn per_node_tx_counts_are_independent() {
    let mut m = MetricsCollector::new();
    m.record_tx(NodeId(1));
    m.record_tx(NodeId(1));
    m.record_tx(NodeId(2));
    m.record_tx(NodeId(3));
    m.record_tx(NodeId(3));
    m.record_tx(NodeId(3));

    assert_eq!(m.node_tx_count(NodeId(1)), 2);
    assert_eq!(m.node_tx_count(NodeId(2)), 1);
    assert_eq!(m.node_tx_count(NodeId(3)), 3);
    assert_eq!(m.total_tx, 6);
}

#[test]
fn per_node_rx_counts_are_independent() {
    let mut m = MetricsCollector::new();
    m.record_rx(NodeId(10));
    m.record_rx(NodeId(10));
    m.record_rx(NodeId(20));

    assert_eq!(m.node_rx_count(NodeId(10)), 2);
    assert_eq!(m.node_rx_count(NodeId(20)), 1);
    assert_eq!(m.node_rx_count(NodeId(99)), 0);
    assert_eq!(m.total_rx, 3);
}

#[test]
fn metrics_tx_and_rx_do_not_interfere() {
    let mut m = MetricsCollector::new();
    m.record_tx(NodeId(1));
    m.record_rx(NodeId(1));

    assert_eq!(m.node_tx_count(NodeId(1)), 1);
    assert_eq!(m.node_rx_count(NodeId(1)), 1);
    assert_eq!(m.total_tx, 1);
    assert_eq!(m.total_rx, 1);
}

// ===========================================================================
// Scheduler event ordering tests
// ===========================================================================

#[test]
fn event_kind_wake_equality() {
    let a = EventKind::Wake {
        node_id: NodeId(1),
    };
    let b = EventKind::Wake {
        node_id: NodeId(1),
    };
    let c = EventKind::Wake {
        node_id: NodeId(2),
    };
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn event_kind_tx_complete_equality() {
    let a = EventKind::TxComplete {
        sender: NodeId(1),
    };
    let b = EventKind::TxComplete {
        sender: NodeId(1),
    };
    assert_eq!(a, b);
}

#[test]
fn event_kind_variants_are_distinct() {
    let wake = EventKind::Wake {
        node_id: NodeId(1),
    };
    let tx = EventKind::TxComplete {
        sender: NodeId(1),
    };
    let poll = EventKind::InterferencePoll { interferer_idx: 0 };
    assert_ne!(wake, tx);
    assert_ne!(wake, poll);
    assert_ne!(tx, poll);
}

// ===========================================================================
// Scheduler: multi-node interleaving
// ===========================================================================

#[test]
fn two_periodic_nodes_both_transmit() {
    let mut sched = Scheduler::new(500_000);
    // Node 1 wakes every 100_000us, Node 2 every 150_000us.
    // They transmit on different frequencies to avoid collision.
    let node1 = CountingNode::new(1, 100_000, Some(make_tx(7, 868_100_000, 30_000, 14)));
    let node2 = CountingNode::new(2, 150_000, Some(make_tx(7, 868_300_000, 30_000, 14)));
    sched.add_node(Box::new(node1), Some(0));
    sched.add_node(Box::new(node2), Some(0));
    sched.run();

    // Node 1: wakes at 0, 100k, 200k, 300k, 400k = 5 TXs
    // Node 2: wakes at 0, 150k, 300k, 450k = 4 TXs
    assert_eq!(sched.metrics.node_tx_count(NodeId(1)), 5);
    assert_eq!(sched.metrics.node_tx_count(NodeId(2)), 4);
    assert_eq!(sched.metrics.total_tx, 9);
    assert_eq!(sched.metrics.total_collisions, 0);
}

#[test]
fn scheduler_current_time_advances() {
    let mut sched = Scheduler::new(1_000_000);
    assert_eq!(sched.current_time(), 0);

    let node = CountingNode::new(1, 200_000, None);
    sched.add_node(Box::new(node), Some(100_000));
    sched.run();

    // Node wakes at 100k, 300k, 500k, 700k, 900k; next would be 1100k > end
    assert_eq!(sched.current_time(), 900_000);
}

#[test]
fn scheduler_with_no_nodes_terminates_immediately() {
    let mut sched = Scheduler::new(1_000_000);
    sched.run();
    assert_eq!(sched.current_time(), 0);
    assert_eq!(sched.metrics.total_tx, 0);
}

#[test]
fn scheduler_with_no_events_terminates() {
    let mut sched = Scheduler::new(1_000_000);
    sched.add_node(Box::new(InertNode { id: NodeId(1) }), None);
    sched.add_node(Box::new(InertNode { id: NodeId(2) }), None);
    sched.run();
    assert_eq!(sched.current_time(), 0);
}

#[test]
fn node_without_tx_still_receives() {
    let mut sched = Scheduler::new(200_000);
    // Sender transmits exactly once
    let sender = OneShotNode::new(1, default_tx());
    let listener = InertNode { id: NodeId(2) };
    sched.add_node(Box::new(sender), Some(0));
    sched.add_node(Box::new(listener), None);
    sched.run();

    assert_eq!(sched.metrics.total_tx, 1);
    assert_eq!(sched.metrics.node_rx_count(NodeId(2)), 1);
    assert_eq!(sched.metrics.node_rx_count(NodeId(1)), 0);
}

// ===========================================================================
// Channel: capture-effect edge cases
// ===========================================================================

#[test]
fn capture_weaker_arrives_first() {
    let mut ch = Channel::new();
    let weak = make_tx(7, 868_100_000, 50_000, 8);
    let strong = make_tx(7, 868_100_000, 50_000, 20);
    ch.begin_transmission(NodeId(1), &weak, 0);
    ch.begin_transmission(NodeId(2), &strong, 10_000);
    ch.resolve_at(60_000);

    let completed = ch.drain_completed();
    let weak_entry = completed
        .iter()
        .find(|(id, _, _, _)| *id == NodeId(1))
        .unwrap();
    let strong_entry = completed
        .iter()
        .find(|(id, _, _, _)| *id == NodeId(2))
        .unwrap();

    assert!(weak_entry.1, "weak (arrived first) should be collided");
    assert!(!strong_entry.1, "strong (arrived second) should survive");
    assert!(strong_entry.2, "strong should be marked captured");
}

#[test]
fn with_co_channel_rejection_affects_capture_threshold() {
    let mut ch = Channel::with_co_channel_rejection(50.0);
    let strong = make_tx(7, 868_100_000, 50_000, 20);
    let weak = make_tx(7, 868_100_000, 50_000, -10);
    ch.begin_transmission(NodeId(1), &strong, 0);
    ch.begin_transmission(NodeId(2), &weak, 10_000);
    ch.resolve_at(60_000);

    let delivered = ch.deliver_to(60_000);
    assert_eq!(
        delivered.len(),
        0,
        "delta=30 < threshold=50, both should collide"
    );
}

// ===========================================================================
// Channel: RSSI/SNR values in delivered frames
// ===========================================================================

#[test]
fn delivered_frame_has_plausible_rssi_and_snr() {
    let mut ch = Channel::new();
    let tx = make_tx(10, 868_100_000, 80_000, 14);
    ch.begin_transmission(NodeId(1), &tx, 0);
    ch.resolve_at(80_000);

    let delivered = ch.deliver_to(80_000);
    assert_eq!(delivered.len(), 1);

    let frame = &delivered[0];
    // RSSI = 14 - 100 = -86
    assert!((frame.rssi - (-86.0_f32)).abs() < 0.001);
    // SNR = -86 - (-117) = 31
    assert!((frame.snr - 31.0_f32).abs() < 0.001);
    assert_eq!(frame.sf, 10);
    assert_eq!(frame.frequency, 868_100_000);
    assert_eq!(frame.time, 80_000);
}

// ===========================================================================
// Proptests
// ===========================================================================

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// RSSI must increase monotonically with TX power.
        #[test]
        fn rssi_monotone_with_tx_power(a in -128i8..=126i8, b in -128i8..=126i8) {
            let ch = Channel::new();
            let rssi_a = ch.compute_rssi(a);
            let rssi_b = ch.compute_rssi(b);
            if a <= b {
                prop_assert!(rssi_a <= rssi_b);
            } else {
                prop_assert!(rssi_a > rssi_b);
            }
        }

        /// SNR must increase monotonically with RSSI.
        #[test]
        fn snr_monotone_with_rssi(a in -200.0f32..200.0f32, b in -200.0f32..200.0f32) {
            let ch = Channel::new();
            let snr_a = ch.compute_snr(a);
            let snr_b = ch.compute_snr(b);
            if a <= b {
                prop_assert!(snr_a <= snr_b);
            } else {
                prop_assert!(snr_a > snr_b);
            }
        }

        /// Per-node TX counts always sum to total_tx.
        #[test]
        fn per_node_tx_sums_to_total(
            counts in prop::collection::vec(1u64..50, 1..10)
        ) {
            let mut m = MetricsCollector::new();
            let mut expected_total = 0u64;
            for (i, &count) in counts.iter().enumerate() {
                for _ in 0..count {
                    m.record_tx(NodeId(i as u32));
                    expected_total += 1;
                }
            }
            prop_assert_eq!(m.total_tx, expected_total);
            let sum: u64 = (0..counts.len())
                .map(|i| m.node_tx_count(NodeId(i as u32)))
                .sum();
            prop_assert_eq!(sum, expected_total);
        }

        /// Per-node RX counts always sum to total_rx.
        #[test]
        fn per_node_rx_sums_to_total(
            counts in prop::collection::vec(1u64..50, 1..10)
        ) {
            let mut m = MetricsCollector::new();
            let mut expected_total = 0u64;
            for (i, &count) in counts.iter().enumerate() {
                for _ in 0..count {
                    m.record_rx(NodeId(i as u32));
                    expected_total += 1;
                }
            }
            prop_assert_eq!(m.total_rx, expected_total);
            let sum: u64 = (0..counts.len())
                .map(|i| m.node_rx_count(NodeId(i as u32)))
                .sum();
            prop_assert_eq!(sum, expected_total);
        }

        /// A single TX in an otherwise empty channel never collides.
        #[test]
        fn solo_tx_never_collides(
            sf in 7u8..13u8,
            freq in prop::sample::select(vec![868_100_000u32, 868_300_000, 868_500_000]),
            duration in 10_000u64..200_000,
            power in -10i8..20
        ) {
            let mut ch = Channel::new();
            let tx = make_tx(sf, freq, duration, power);
            ch.begin_transmission(NodeId(1), &tx, 0);
            ch.resolve_at(duration);
            let completed = ch.drain_completed();
            prop_assert_eq!(completed.len(), 1);
            prop_assert!(!completed[0].1, "solo TX must never collide");
        }

        /// Resolve at exactly the end time always completes the TX.
        #[test]
        fn resolve_at_exact_end_completes(
            duration in 1_000u64..500_000
        ) {
            let mut ch = Channel::new();
            let tx = make_tx(7, 868_100_000, duration, 14);
            ch.begin_transmission(NodeId(1), &tx, 0);
            let events = ch.resolve_at(duration);
            prop_assert_eq!(events.len(), 1);
        }
    }
}
