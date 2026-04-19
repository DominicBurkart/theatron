//! Tests for the InterferenceSource::observe callback and related invariants.
//!
//! These tests cover gaps identified in the existing test suite:
//!
//! 1. `InterferenceSource::observe` receives the correct `ChannelEvent` variants
//!    and fields when the scheduler delivers TX-started and TX-completed events.
//! 2. Collision symmetry: every collision marks both participating transmissions,
//!    so `total_collisions` is always even when only pairwise collisions occur.
//! 3. `Channel::with_co_channel_rejection` preserves all other LoRa defaults.
//! 4. A receive-triggered wake fires at the exact scheduled time and produces
//!    a follow-up transmission.
//! 5. `MetricsCollector::total_captures` and `total_collisions` are independent
//!    and can both be non-zero simultaneously (capture-effect scenario).

use std::cell::RefCell;
use std::rc::Rc;

use theatron::channel::{Channel, ChannelConfig, LORA_CO_CHANNEL_REJECTION_DB, LORA_NOISE_FLOOR_DBM, LORA_PATH_LOSS_DB};
use theatron::metrics::MetricsCollector;
use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_tx(sf: u8, freq: u32, dur: u64, power: i8) -> Transmission {
    Transmission {
        payload: vec![0xBB],
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency: freq,
        duration_us: dur,
        tx_power_dbm: power,
    }
}

/// A node that optionally queues one transmission on its initial wake.
struct OneShotNode {
    id: NodeId,
    tx: Option<Transmission>,
}

impl OneShotNode {
    fn new(id: u32, tx: Option<Transmission>) -> Self {
        Self { id: NodeId(id), tx }
    }
}

impl NodeHandle for OneShotNode {
    fn node_id(&self) -> NodeId { self.id }
    fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> { None }
    fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> { self.tx.take() }
    fn update(&mut self, _t: SimTime) -> Option<SimTime> { None }
}

/// An interferer that records every `ChannelEvent` it observes.
struct RecordingInterferer {
    observed: Vec<ChannelEvent>,
    inject_once: Option<Transmission>,
}

impl RecordingInterferer {
    fn new() -> Self {
        Self { observed: Vec::new(), inject_once: None }
    }

    fn with_inject(tx: Transmission) -> Self {
        Self { observed: Vec::new(), inject_once: Some(tx) }
    }
}

impl InterferenceSource for RecordingInterferer {
    fn observe(&mut self, event: &ChannelEvent, _time: SimTime) {
        self.observed.push(event.clone());
    }

    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        self.inject_once.take()
    }

    fn next_poll_time(&self, _current_time: SimTime) -> Option<SimTime> {
        None
    }
}

// ---------------------------------------------------------------------------
// observe callback tests
// ---------------------------------------------------------------------------

/// The scheduler must call `observe` with a `TransmissionStarted` event when a
/// node begins a transmission, and with a `TransmissionCompleted` event when it
/// ends. An interferer registered before the node TX observes both.
#[test]
fn observe_receives_started_and_completed_events() {
    // We use a raw pointer to peek at the interferer's recorded events after
    // the run. The interferer is moved into the scheduler, so we extract the
    // data via a shared Rc<RefCell<…>> instead.
    let observed: Rc<RefCell<Vec<ChannelEvent>>> = Rc::new(RefCell::new(Vec::new()));

    struct SharedRecorder {
        observed: Rc<RefCell<Vec<ChannelEvent>>>,
        inject_once: Option<Transmission>,
    }

    impl InterferenceSource for SharedRecorder {
        fn observe(&mut self, event: &ChannelEvent, _time: SimTime) {
            self.observed.borrow_mut().push(event.clone());
        }
        fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
            self.inject_once.take()
        }
        fn next_poll_time(&self, _: SimTime) -> Option<SimTime> {
            None
        }
    }

    let mut sched = Scheduler::new(200_000);
    let node = OneShotNode::new(1, Some(make_tx(7, 868_100_000, 50_000, 14)));
    sched.add_node(Box::new(node), Some(0));
    sched.add_interferer(
        Box::new(SharedRecorder {
            observed: Rc::clone(&observed),
            inject_once: None,
        }),
        0,
    );
    sched.run();

    let events = observed.borrow();
    // Expect at least one TransmissionStarted and one TransmissionCompleted.
    let started = events
        .iter()
        .filter(|e| matches!(e, ChannelEvent::TransmissionStarted { .. }))
        .count();
    let completed = events
        .iter()
        .filter(|e| matches!(e, ChannelEvent::TransmissionCompleted { .. }))
        .count();

    assert!(started >= 1, "observe must be called with TransmissionStarted");
    assert!(completed >= 1, "observe must be called with TransmissionCompleted");
}

/// The `TransmissionStarted` event delivered to `observe` must carry the
/// correct sender, SF, frequency, and start time.
#[test]
fn observe_started_event_carries_correct_fields() {
    const SF: u8 = 9;
    const FREQ: u32 = 868_300_000;
    const START_TIME: SimTime = 0;

    let observed: Rc<RefCell<Vec<ChannelEvent>>> = Rc::new(RefCell::new(Vec::new()));

    struct FieldChecker {
        observed: Rc<RefCell<Vec<ChannelEvent>>>,
    }

    impl InterferenceSource for FieldChecker {
        fn observe(&mut self, event: &ChannelEvent, _time: SimTime) {
            self.observed.borrow_mut().push(event.clone());
        }
        fn poll_inject(&mut self, _: SimTime) -> Option<Transmission> { None }
        fn next_poll_time(&self, _: SimTime) -> Option<SimTime> { None }
    }

    let mut sched = Scheduler::new(200_000);
    sched.add_node(
        Box::new(OneShotNode::new(7, Some(make_tx(SF, FREQ, 50_000, 14)))),
        Some(START_TIME),
    );
    sched.add_interferer(
        Box::new(FieldChecker { observed: Rc::clone(&observed) }),
        0,
    );
    sched.run();

    let events = observed.borrow();
    let started = events
        .iter()
        .find(|e| matches!(e, ChannelEvent::TransmissionStarted { .. }))
        .expect("must have at least one TransmissionStarted event");

    match started {
        ChannelEvent::TransmissionStarted { sender, sf, frequency, time } => {
            assert_eq!(*sender, NodeId(7));
            assert_eq!(*sf, SF);
            assert_eq!(*frequency, FREQ);
            assert_eq!(*time, START_TIME);
        }
        _ => unreachable!(),
    }
}

/// An interferer that itself injects a transmission should also receive
/// `observe` callbacks for that injection (interferers observe each other).
#[test]
fn observe_called_for_interferer_own_injection() {
    let observed: Rc<RefCell<Vec<ChannelEvent>>> = Rc::new(RefCell::new(Vec::new()));

    struct SelfObservingInterferer {
        observed: Rc<RefCell<Vec<ChannelEvent>>>,
        injected: bool,
    }

    impl InterferenceSource for SelfObservingInterferer {
        fn observe(&mut self, event: &ChannelEvent, _time: SimTime) {
            self.observed.borrow_mut().push(event.clone());
        }
        fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
            if !self.injected {
                self.injected = true;
                Some(make_tx(7, 868_100_000, 30_000, 14))
            } else {
                None
            }
        }
        fn next_poll_time(&self, _: SimTime) -> Option<SimTime> { None }
    }

    let mut sched = Scheduler::new(200_000);
    sched.add_interferer(
        Box::new(SelfObservingInterferer {
            observed: Rc::clone(&observed),
            injected: false,
        }),
        0,
    );
    sched.run();

    let events = observed.borrow();
    // The interferer injects one TX. It should see the TransmissionStarted for it.
    let started_count = events
        .iter()
        .filter(|e| matches!(e, ChannelEvent::TransmissionStarted { .. }))
        .count();
    assert!(started_count >= 1, "interferer must observe its own injection start");
}

// ---------------------------------------------------------------------------
// Collision symmetry
// ---------------------------------------------------------------------------

/// When two transmissions collide, both are marked collided — so
/// `total_collisions` must be even for pairwise collision scenarios.
/// This tests the invariant across a range of simultaneous-sender counts.
proptest! {
    #[test]
    fn collision_count_is_always_even_for_equal_power_simultaneous_txs(
        n in 2usize..8
    ) {
        let mut sched = Scheduler::new(200_000);
        for i in 0..n {
            let node = OneShotNode::new(i as u32, Some(make_tx(7, 868_100_000, 50_000, 14)));
            sched.add_node(Box::new(node), Some(0));
        }
        sched.run();
        prop_assert_eq!(
            sched.metrics.total_collisions % 2, 0,
            "total_collisions must be even: got {}",
            sched.metrics.total_collisions
        );
    }
}

/// Two simultaneous equal-power transmissions on the same SF/freq produce
/// exactly 2 collision records (one per sender).
#[test]
fn two_simultaneous_txs_produce_exactly_two_collision_records() {
    let mut sched = Scheduler::new(200_000);
    sched.add_node(Box::new(OneShotNode::new(1, Some(make_tx(7, 868_100_000, 50_000, 14)))), Some(0));
    sched.add_node(Box::new(OneShotNode::new(2, Some(make_tx(7, 868_100_000, 50_000, 14)))), Some(0));
    sched.run();
    assert_eq!(sched.metrics.total_collisions, 2);
    assert_eq!(sched.metrics.total_rx, 0);
}

// ---------------------------------------------------------------------------
// Channel::with_co_channel_rejection config propagation
// ---------------------------------------------------------------------------

/// `with_co_channel_rejection` must change only the co-channel rejection
/// threshold; path_loss_db and noise_floor_dbm must remain at LoRa defaults.
#[test]
fn with_co_channel_rejection_preserves_other_lora_defaults() {
    let custom_db = 12.0_f32;
    let ch = Channel::with_co_channel_rejection(custom_db);
    let cfg = ch.config();

    assert_eq!(cfg.co_channel_rejection_db, custom_db);
    assert_eq!(cfg.path_loss_db, LORA_PATH_LOSS_DB,
        "path_loss_db must remain at LoRa default");
    assert_eq!(cfg.noise_floor_dbm, LORA_NOISE_FLOOR_DBM,
        "noise_floor_dbm must remain at LoRa default");
}

/// The default co-channel rejection threshold matches the LoRa constant.
#[test]
fn with_co_channel_rejection_default_matches_lora_constant() {
    let ch = Channel::with_co_channel_rejection(LORA_CO_CHANNEL_REJECTION_DB);
    assert_eq!(*ch.config(), ChannelConfig::lora_defaults());
}

// ---------------------------------------------------------------------------
// Receive-triggered wake fires at correct time and produces a follow-up TX
// ---------------------------------------------------------------------------

/// When `on_receive` returns `Some(t)`, the scheduler must call `update` at
/// exactly `t`, and any transmission queued in that `update` must be recorded.
#[test]
fn receive_triggered_wake_fires_at_exact_time_and_tx_recorded() {
    const WAKE_DELAY_US: SimTime = 50_000;
    const TX_DURATION_US: u64 = 20_000;

    let wake_time_seen: Rc<RefCell<Option<SimTime>>> = Rc::new(RefCell::new(None));

    struct DelayedReplier {
        id: NodeId,
        wake_delay_us: SimTime,
        wake_time_seen: Rc<RefCell<Option<SimTime>>>,
        pending_tx: Option<Transmission>,
        replied: bool,
    }

    impl NodeHandle for DelayedReplier {
        fn node_id(&self) -> NodeId { self.id }

        fn on_receive(&mut self, _f: RxMetadata, time: SimTime) -> Option<SimTime> {
            if !self.replied {
                Some(time + self.wake_delay_us)
            } else {
                None
            }
        }

        fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> {
            self.pending_tx.take()
        }

        fn update(&mut self, time: SimTime) -> Option<SimTime> {
            if !self.replied {
                self.replied = true;
                *self.wake_time_seen.borrow_mut() = Some(time);
                self.pending_tx = Some(Transmission {
                    payload: vec![0xFF],
                    sf: 7,
                    bandwidth: 125_000,
                    coding_rate: 5,
                    frequency: 868_100_000,
                    duration_us: TX_DURATION_US,
                    tx_power_dbm: 14,
                });
            }
            None
        }
    }

    let mut sched = Scheduler::new(1_000_000);
    // Sender transmits at t=0, duration=50_000 → completes at t=50_000.
    sched.add_node(
        Box::new(OneShotNode::new(1, Some(make_tx(7, 868_100_000, 50_000, 14)))),
        Some(0),
    );
    // DelayedReplier receives at t=50_000, schedules wake at t=50_000+50_000=100_000.
    sched.add_node(
        Box::new(DelayedReplier {
            id: NodeId(2),
            wake_delay_us: WAKE_DELAY_US,
            wake_time_seen: Rc::clone(&wake_time_seen),
            pending_tx: None,
            replied: false,
        }),
        None,
    );
    sched.run();

    // Original sender + delayed reply = 2 transmissions.
    assert_eq!(sched.metrics.total_tx, 2, "original TX + delayed reply must both be recorded");

    // The wake must have fired at exactly 50_000 (receive time) + 50_000 (delay).
    let observed = *wake_time_seen.borrow();
    assert_eq!(
        observed,
        Some(100_000),
        "wake must fire at receive_time + delay: got {:?}, expected Some(100_000)",
        observed,
    );
}

// ---------------------------------------------------------------------------
// MetricsCollector: captures and collisions are independent
// ---------------------------------------------------------------------------

/// In a capture-effect scenario, `total_captures` and `total_collisions` are
/// both non-zero simultaneously. They count different things:
/// - `total_collisions`: frames that were lost due to a collision.
/// - `total_captures`: frames that survived despite a simultaneous transmission.
#[test]
fn captures_and_collisions_are_independent_and_can_coexist() {
    // strong (20 dBm) vs weak (14 dBm) on same SF/freq: delta=6 >= threshold=6
    // → strong is captured (non-collided), weak is collided.
    let mut sched = Scheduler::new(200_000);
    sched.add_node(
        Box::new(OneShotNode::new(1, Some(make_tx(7, 868_100_000, 50_000, 20)))),
        Some(0),
    );
    sched.add_node(
        Box::new(OneShotNode::new(2, Some(make_tx(7, 868_100_000, 50_000, 14)))),
        Some(0),
    );
    // Passive receiver to give the scheduler somewhere to deliver the captured frame.
    sched.add_node(Box::new(OneShotNode::new(3, None)), None);
    sched.run();

    assert_eq!(sched.metrics.total_captures, 1, "one capture event expected");
    assert_eq!(sched.metrics.total_collisions, 1, "one collision event expected (weak sender)");
    // Both counters non-zero simultaneously:
    assert!(sched.metrics.total_captures > 0 && sched.metrics.total_collisions > 0);
}

/// MetricsCollector accumulates captures and collisions independently from
/// individual `record_capture` / `record_collision` calls.
#[test]
fn metrics_capture_and_collision_independent_counters() {
    let mut m = MetricsCollector::new();
    m.record_capture();
    m.record_capture();
    m.record_collision();

    assert_eq!(m.total_captures, 2);
    assert_eq!(m.total_collisions, 1);
    // Mutating one must not affect the other.
    m.record_collision();
    assert_eq!(m.total_captures, 2, "capture count must not change when recording collision");
    assert_eq!(m.total_collisions, 2);
}
