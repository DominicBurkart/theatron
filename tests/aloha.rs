use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::types::{NodeId, RxMetadata, Transmission};

// --- Test helpers ---
//
// NOTE: These tests use local `PeriodicSender` / `Receiver` helpers rather than
// `AlohaNode`/`AlohaReceiver` from `examples/aloha/aloha_node.rs`. This is
// intentional for now: the example types are compiled only as part of the
// `aloha` example binary (not as a library), so they cannot be `use`-imported
// from integration tests without additional crate restructuring.
//
// As a result, these tests validate that the *scheduler and channel model*
// correctly handle ALOHA-like transmission patterns (collision, SF orthogonality,
// frequency orthogonality, sequential delivery). They do NOT exercise `AlohaNode`
// end-to-end. Wiring the integration tests to `AlohaNode` is tracked as a
// follow-up: it requires either moving shared types into the library crate or
// using `#[path = "../examples/aloha/aloha_node.rs"] mod aloha_node;` (which
// becomes meaningful once `AlohaNode` has retransmission logic worth testing).

fn make_tx(payload: Vec<u8>, sf: u8, frequency: u32, duration_us: u64) -> Transmission {
    Transmission {
        payload,
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency,
        duration_us,
        tx_power_dbm: 14,
    }
}

fn make_tx_power(
    payload: Vec<u8>,
    sf: u8,
    frequency: u32,
    duration_us: u64,
    tx_power_dbm: i8,
) -> Transmission {
    Transmission {
        payload,
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency,
        duration_us,
        tx_power_dbm,
    }
}

/// A node that transmits a fixed number of packets at regular intervals.
struct PeriodicSender {
    id: NodeId,
    interval_us: u64,
    duration_us: u64,
    remaining: usize,
    sf: u8,
    frequency: u32,
    pending: Option<Transmission>,
}

impl PeriodicSender {
    fn new(
        id: u32,
        interval_us: u64,
        duration_us: u64,
        count: usize,
        sf: u8,
        frequency: u32,
    ) -> Self {
        Self {
            id: NodeId(id),
            interval_us,
            duration_us,
            remaining: count,
            sf,
            frequency,
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
        if self.remaining > 0 {
            self.remaining -= 1;
            self.pending = Some(make_tx(
                vec![self.id.0 as u8; 10],
                self.sf,
                self.frequency,
                self.duration_us,
            ));
            Some(time + self.interval_us)
        } else {
            None
        }
    }
}

/// A sender with a configurable transmit power (dBm), used to test signal
/// capture: when two nodes transmit simultaneously on the same SF/frequency
/// and the power difference meets the co-channel rejection threshold (6 dB by
/// default), the stronger signal is "captured" and delivered while the weaker
/// one is marked collided.
struct PoweredSender {
    id: NodeId,
    sf: u8,
    frequency: u32,
    duration_us: u64,
    tx_power_dbm: i8,
    fired: bool,
}

impl PoweredSender {
    fn new(id: u32, sf: u8, frequency: u32, duration_us: u64, tx_power_dbm: i8) -> Self {
        Self {
            id: NodeId(id),
            sf,
            frequency,
            duration_us,
            tx_power_dbm,
            fired: false,
        }
    }
}

impl NodeHandle for PoweredSender {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        if !self.fired {
            self.fired = true;
            Some(make_tx_power(
                vec![self.id.0 as u8; 4],
                self.sf,
                self.frequency,
                self.duration_us,
                self.tx_power_dbm,
            ))
        } else {
            None
        }
    }

    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

/// A passive receiver that counts received frames.
struct Receiver {
    id: NodeId,
    count: usize,
}

impl Receiver {
    fn new(id: u32) -> Self {
        Self {
            id: NodeId(id),
            count: 0,
        }
    }
}

impl NodeHandle for Receiver {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        self.count += 1;
        None
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        None
    }

    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

// --- Tests ---

/// A single ALOHA sender with no contention should deliver all packets.
///
/// Sender fires at t=0, 1s, 2s, 3s, 4s (count=5, interval=1s) — all within
/// the 20s window, so exactly 5 TXs are expected.
#[test]
fn single_sender_all_delivered() {
    let mut sched = Scheduler::new(20_000_000);
    let sender = PeriodicSender::new(1, 1_000_000, 50_000, 5, 7, 868_100_000);
    let receiver = Receiver::new(99);
    sched.add_node(Box::new(sender), Some(0));
    sched.add_node(Box::new(receiver), None);
    sched.run();
    assert_eq!(sched.metrics.total_tx, 5);
    // Each TX delivered to 1 receiver
    assert_eq!(sched.metrics.total_rx, 5);
    assert_eq!(sched.metrics.total_collisions, 0);
}

/// Two senders transmitting simultaneously on the same SF/frequency should collide.
#[test]
fn two_simultaneous_senders_collide() {
    let mut sched = Scheduler::new(1_000_000);
    // Both transmit at t=0 with 200ms duration on same SF/freq
    let sender1 = PeriodicSender::new(1, 500_000, 200_000, 1, 7, 868_100_000);
    let sender2 = PeriodicSender::new(2, 500_000, 200_000, 1, 7, 868_100_000);
    let receiver = Receiver::new(99);
    sched.add_node(Box::new(sender1), Some(0));
    sched.add_node(Box::new(sender2), Some(0));
    sched.add_node(Box::new(receiver), None);
    sched.run();
    assert_eq!(sched.metrics.total_tx, 2);
    assert!(
        sched.metrics.total_collisions >= 1,
        "simultaneous same-SF/freq transmissions should collide"
    );
}

/// Senders on different frequencies should not collide.
#[test]
fn different_frequencies_no_collision() {
    let mut sched = Scheduler::new(1_000_000);
    let sender1 = PeriodicSender::new(1, 500_000, 200_000, 1, 7, 868_100_000);
    let sender2 = PeriodicSender::new(2, 500_000, 200_000, 1, 7, 868_300_000);
    let receiver = Receiver::new(99);
    sched.add_node(Box::new(sender1), Some(0));
    sched.add_node(Box::new(sender2), Some(0));
    sched.add_node(Box::new(receiver), None);
    sched.run();
    assert_eq!(sched.metrics.total_tx, 2);
    // Each TX delivered to 2 non-sender nodes (the other sender + receiver)
    assert_eq!(sched.metrics.total_rx, 4);
    assert_eq!(sched.metrics.total_collisions, 0);
}

/// Senders on different SFs should not collide (SF orthogonality).
#[test]
fn different_sf_no_collision() {
    let mut sched = Scheduler::new(1_000_000);
    let sender1 = PeriodicSender::new(1, 500_000, 200_000, 1, 7, 868_100_000);
    let sender2 = PeriodicSender::new(2, 500_000, 200_000, 1, 12, 868_100_000);
    let receiver = Receiver::new(99);
    sched.add_node(Box::new(sender1), Some(0));
    sched.add_node(Box::new(sender2), Some(0));
    sched.add_node(Box::new(receiver), None);
    sched.run();
    assert_eq!(sched.metrics.total_tx, 2);
    // Each TX delivered to 2 non-sender nodes
    assert_eq!(sched.metrics.total_rx, 4);
    assert_eq!(sched.metrics.total_collisions, 0);
}

/// Non-overlapping transmissions on the same channel should all succeed.
#[test]
fn sequential_transmissions_no_collision() {
    let mut sched = Scheduler::new(20_000_000);
    // Sender 1 transmits at t=0, sender 2 at t=1s — no overlap with 200ms duration
    let sender1 = PeriodicSender::new(1, 2_000_000, 200_000, 3, 7, 868_100_000);
    let sender2 = PeriodicSender::new(2, 2_000_000, 200_000, 3, 7, 868_100_000);
    let receiver = Receiver::new(99);
    sched.add_node(Box::new(sender1), Some(0));
    sched.add_node(Box::new(sender2), Some(1_000_000));
    sched.add_node(Box::new(receiver), None);
    sched.run();
    assert_eq!(sched.metrics.total_tx, 6);
    assert_eq!(sched.metrics.total_collisions, 0);
}

/// Multiple senders all transmitting at the same time should cause collisions,
/// demonstrating the classic ALOHA throughput problem.
#[test]
fn five_simultaneous_senders_high_collision_rate() {
    let mut sched = Scheduler::new(1_000_000);
    for i in 1..=5u32 {
        let sender = PeriodicSender::new(i, 500_000, 200_000, 1, 7, 868_100_000);
        sched.add_node(Box::new(sender), Some(0));
    }
    let receiver = Receiver::new(99);
    sched.add_node(Box::new(receiver), None);
    sched.run();
    assert_eq!(sched.metrics.total_tx, 5);
    assert!(
        sched.metrics.total_collisions >= 1,
        "5 simultaneous senders must produce collisions"
    );
    // With 5 equal-power simultaneous TXs, all collide → zero deliveries
    assert_eq!(sched.metrics.total_rx, 0);
}

/// When two nodes transmit simultaneously on the same SF/frequency but one is
/// at least 6 dB stronger (the default co-channel rejection threshold), the
/// stronger signal is captured and delivered while the weaker one is lost.
/// This exercises the `metrics.record_capture()` path in the scheduler.
#[test]
fn capture_effect_recorded_in_metrics() {
    let mut sched = Scheduler::new(1_000_000);
    // strong: 20 dBm, weak: 14 dBm → delta = 6 dB == threshold → capture
    let strong = PoweredSender::new(1, 7, 868_100_000, 200_000, 20);
    let weak = PoweredSender::new(2, 7, 868_100_000, 200_000, 14);
    let receiver = Receiver::new(99);
    sched.add_node(Box::new(strong), Some(0));
    sched.add_node(Box::new(weak), Some(0));
    sched.add_node(Box::new(receiver), None);
    sched.run();
    assert_eq!(sched.metrics.total_tx, 2);
    // The strong signal is captured: one frame delivered, one collision.
    // The captured frame is delivered to all non-sender nodes: the weak sender
    // (NodeId(2)) and the receiver (NodeId(99)) — consistent with how the
    // scheduler delivers any successful TX to every non-sender node.
    assert_eq!(sched.metrics.total_captures, 1, "expected one capture event");
    assert_eq!(
        sched.metrics.total_collisions,
        1,
        "weak sender should be marked collided"
    );
    assert_eq!(
        sched.metrics.total_rx,
        2,
        "captured frame delivered to 2 non-sender nodes (weak sender + receiver)"
    );
}
