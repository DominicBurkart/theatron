//! End-to-end invariant tests targeting two undertested scheduler integration paths:
//!
//! 1. **`on_receive` wake-scheduling**: when `on_receive` returns `Some(t)`, the
//!    scheduler must call `update` at exactly time `t`. This is the receive-side
//!    counterpart of the timer contract already covered for `update`-returned wakes
//!    in [`tests/protocol_timer_contract.rs`]. The path through
//!    `Scheduler::deliver_completed_to_nodes` (the `if let Some(t) = wake` branch
//!    inside the receiver loop in `src/scheduler.rs`) is exercised by existing
//!    tests but no test pins down that the wake actually fires at the requested
//!    time. Without this guarantee, RX-window-based protocols (LoRaWAN Class A's
//!    RX1/RX2 windows after a downlink) cannot be validated end-to-end.
//!
//! 2. **Interferer-vs-interferer collisions and cross-observation**: when two
//!    interferers transmit overlapping frames on the same SF/frequency, both must
//!    be marked collided, and *every* interferer (including the colliding ones)
//!    must observe `TransmissionStarted` for both injections plus
//!    `TransmissionCompleted` events for them. Existing tests cover one-injection
//!    cross-observation (`all_interferers_observe_interferer_originated_events`)
//!    but not the collision case relied upon by the adversarial-replay and
//!    co-channel-contention models from `ARCHITECTURE.md`.

use std::cell::RefCell;
use std::rc::Rc;

use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tx(sf: u8, freq: u32, dur: u64, power: i8) -> Transmission {
    Transmission {
        payload: vec![0xAB],
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency: freq,
        duration_us: dur,
        tx_power_dbm: power,
    }
}

// ---------------------------------------------------------------------------
// 1. on_receive wake-scheduling integration
// ---------------------------------------------------------------------------

/// A node that, on the first `on_receive`, returns `Some(absolute_wake_time)`
/// and records every `update` invocation. The receive time and the absolute
/// wake time the test passes in are both recorded so we can assert the exact
/// time at which `update` fires after the receive.
struct ReceiveThenWake {
    id: NodeId,
    wake_at: SimTime,
    /// Times at which `on_receive` has been called.
    rx_times: Rc<RefCell<Vec<SimTime>>>,
    /// Times at which `update` has been called.
    update_times: Rc<RefCell<Vec<SimTime>>>,
    /// Whether the wake has been requested yet.
    armed: bool,
}

impl NodeHandle for ReceiveThenWake {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, _frame: RxMetadata, time: SimTime) -> Option<SimTime> {
        self.rx_times.borrow_mut().push(time);
        if self.armed {
            // Already armed: don't reschedule, otherwise we'd loop.
            return None;
        }
        self.armed = true;
        Some(self.wake_at)
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        None
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        self.update_times.borrow_mut().push(time);
        None
    }
}

/// One-shot transmitter helper.
struct OneShotTx {
    id: NodeId,
    pending: Option<Transmission>,
}

impl NodeHandle for OneShotTx {
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

/// When `on_receive` returns `Some(t)`, the scheduler must subsequently call
/// `update` on that node at exactly `t` — not before, not after, not at all
/// only if `t > end_time`.
#[test]
fn on_receive_wake_fires_at_exact_absolute_time() {
    // The TX completes at 50_000us, so the receiver's on_receive fires at 50_000us.
    // The receiver requests an absolute wake at 200_000us. update must fire at 200_000us.
    const TX_DURATION_US: u64 = 50_000;
    const ABS_WAKE: SimTime = 200_000;

    let rx_times = Rc::new(RefCell::new(Vec::new()));
    let update_times = Rc::new(RefCell::new(Vec::new()));

    let mut sched = Scheduler::new(500_000);
    sched.add_node(
        Box::new(OneShotTx {
            id: NodeId(1),
            pending: Some(tx(7, 868_100_000, TX_DURATION_US, 14)),
        }),
        Some(0),
    );
    sched.add_node(
        Box::new(ReceiveThenWake {
            id: NodeId(2),
            wake_at: ABS_WAKE,
            rx_times: Rc::clone(&rx_times),
            update_times: Rc::clone(&update_times),
            armed: false,
        }),
        None,
    );
    sched.run();

    // The TX ends at 50_000, so on_receive fires at 50_000.
    assert_eq!(
        *rx_times.borrow(),
        vec![TX_DURATION_US],
        "on_receive must fire exactly at TX completion time",
    );

    // update must fire exactly at the absolute time the receiver returned.
    assert_eq!(
        *update_times.borrow(),
        vec![ABS_WAKE],
        "update must fire at the exact absolute SimTime returned by on_receive",
    );

    assert!(
        sched.current_time() >= ABS_WAKE,
        "scheduler must advance to at least the requested wake",
    );
}

/// If `on_receive` returns `Some(t)` where `t > end_time`, `update` must NOT
/// fire — the scheduler stops at `end_time`. This pins down the boundary
/// condition for a wake scheduled past the simulation horizon.
#[test]
fn on_receive_wake_beyond_end_time_does_not_fire() {
    let rx_times = Rc::new(RefCell::new(Vec::new()));
    let update_times = Rc::new(RefCell::new(Vec::new()));

    // end_time is 100_000us; receiver asks for wake at 500_000us.
    let mut sched = Scheduler::new(100_000);
    sched.add_node(
        Box::new(OneShotTx {
            id: NodeId(1),
            pending: Some(tx(7, 868_100_000, 50_000, 14)),
        }),
        Some(0),
    );
    sched.add_node(
        Box::new(ReceiveThenWake {
            id: NodeId(2),
            wake_at: 500_000,
            rx_times: Rc::clone(&rx_times),
            update_times: Rc::clone(&update_times),
            armed: false,
        }),
        None,
    );
    sched.run();

    assert_eq!(rx_times.borrow().len(), 1, "on_receive must still fire");
    assert!(
        update_times.borrow().is_empty(),
        "update must not fire when the requested wake is past end_time; got: {:?}",
        update_times.borrow()
    );
    assert!(sched.current_time() <= 100_000);
}

/// When `on_receive` returns `None`, the receiver must not have any later
/// `update` invocation triggered by the receive. This is the negative
/// counterpart to `on_receive_wake_fires_at_exact_absolute_time` and pins down
/// that the scheduler does not synthesize spurious wakes from receives.
#[test]
fn on_receive_returning_none_does_not_schedule_update() {
    /// A receiver that never returns Some from on_receive.
    struct PassiveReceiver {
        id: NodeId,
        update_times: Rc<RefCell<Vec<SimTime>>>,
    }
    impl NodeHandle for PassiveReceiver {
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
            self.update_times.borrow_mut().push(time);
            None
        }
    }

    let update_times = Rc::new(RefCell::new(Vec::new()));
    let mut sched = Scheduler::new(500_000);
    sched.add_node(
        Box::new(OneShotTx {
            id: NodeId(1),
            pending: Some(tx(7, 868_100_000, 50_000, 14)),
        }),
        Some(0),
    );
    sched.add_node(
        Box::new(PassiveReceiver {
            id: NodeId(2),
            update_times: Rc::clone(&update_times),
        }),
        None,
    );
    sched.run();

    assert!(
        update_times.borrow().is_empty(),
        "passive receiver returning None from on_receive must not have update fired; got: {:?}",
        update_times.borrow()
    );
}

// ---------------------------------------------------------------------------
// 2. Interferer-vs-interferer collision and cross-observation
// ---------------------------------------------------------------------------

/// Recorded channel event used in observation assertions.
#[derive(Clone)]
struct ObservedEvent {
    label: &'static str,
    kind: &'static str,
    sender: NodeId,
}

/// An interferer that injects a single transmission at the first poll and
/// records every channel event it observes. Polling stops after the first
/// injection so the simulation winds down deterministically.
struct InjectAndObserve {
    label: &'static str,
    inject: Option<Transmission>,
    observed: Rc<RefCell<Vec<ObservedEvent>>>,
}

impl InterferenceSource for InjectAndObserve {
    fn observe(&mut self, event: &ChannelEvent, _time: SimTime) {
        let (kind, sender) = match event {
            ChannelEvent::TransmissionStarted { sender, .. } => ("started", *sender),
            ChannelEvent::TransmissionCompleted { sender, .. } => ("completed", *sender),
        };
        self.observed.borrow_mut().push(ObservedEvent {
            label: self.label,
            kind,
            sender,
        });
    }

    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        self.inject.take()
    }

    fn next_poll_time(&self, _current_time: SimTime) -> Option<SimTime> {
        None
    }
}

/// Two interferers transmitting overlapping frames on the same SF/frequency
/// must both be marked collided, and *every* interferer must observe the
/// `TransmissionStarted` and `TransmissionCompleted` events for both
/// injections. This verifies the cross-observation invariant for the
/// collision branch of the interferer code path in `Scheduler::run`'s
/// `EventKind::InterferencePoll` arm and the resolution loop in
/// `EventKind::TxComplete`.
#[test]
fn two_interferers_overlap_collide_and_observe_each_other() {
    let observed: Rc<RefCell<Vec<ObservedEvent>>> = Rc::new(RefCell::new(Vec::new()));

    let mut sched = Scheduler::new(300_000);
    let i1 = InjectAndObserve {
        label: "i1",
        inject: Some(tx(7, 868_100_000, 50_000, 14)),
        observed: Rc::clone(&observed),
    };
    let i2 = InjectAndObserve {
        label: "i2",
        inject: Some(tx(7, 868_100_000, 50_000, 14)),
        observed: Rc::clone(&observed),
    };
    // First polls offset so i2's TX overlaps i1's by 40_000us — same SF + freq
    // + power -> both collide.
    sched.add_interferer(Box::new(i1), 0);
    sched.add_interferer(Box::new(i2), 10_000);
    sched.run();

    // Both injected TXs must have collided.
    assert_eq!(
        sched.metrics.total_collisions, 2,
        "two overlapping same-SF/freq/power interferer TXs must both collide"
    );
    // Interferer TXs must contribute to airtime regardless of collision.
    assert_eq!(
        sched.metrics.total_airtime_us, 100_000,
        "both 50_000us TXs must contribute to airtime"
    );

    // Cross-observation: every interferer observes 2 starts + 2 completes.
    let observed = observed.borrow();
    for label in ["i1", "i2"] {
        let started = observed
            .iter()
            .filter(|e| e.label == label && e.kind == "started")
            .count();
        let completed = observed
            .iter()
            .filter(|e| e.label == label && e.kind == "completed")
            .count();
        assert_eq!(
            started, 2,
            "{label} must observe started for both interferer TXs (got {started})"
        );
        assert_eq!(
            completed, 2,
            "{label} must observe completed for both interferer TXs (got {completed})"
        );
    }

    // Synthetic interferer NodeIds occupy the top of the u32 range:
    // first registered -> u32::MAX, second -> u32::MAX - 1.
    let senders_seen: Vec<NodeId> = observed.iter().map(|e| e.sender).collect();
    assert!(
        senders_seen.contains(&NodeId(u32::MAX)),
        "events from the first interferer (synthetic id u32::MAX) must be observed; saw: {senders_seen:?}"
    );
    assert!(
        senders_seen.contains(&NodeId(u32::MAX - 1)),
        "events from the second interferer (synthetic id u32::MAX - 1) must be observed; saw: {senders_seen:?}"
    );
}

/// Two interferers on different SFs must NOT collide, and both must still see
/// each other's events (started + completed). This pins down the orthogonal
/// SF case for interferer-only traffic.
#[test]
fn two_interferers_different_sf_do_not_collide_but_observe() {
    let observed: Rc<RefCell<Vec<ObservedEvent>>> = Rc::new(RefCell::new(Vec::new()));

    let mut sched = Scheduler::new(300_000);
    sched.add_interferer(
        Box::new(InjectAndObserve {
            label: "i1",
            inject: Some(tx(7, 868_100_000, 50_000, 14)),
            observed: Rc::clone(&observed),
        }),
        0,
    );
    sched.add_interferer(
        Box::new(InjectAndObserve {
            label: "i2",
            inject: Some(tx(8, 868_100_000, 50_000, 14)),
            observed: Rc::clone(&observed),
        }),
        10_000,
    );
    sched.run();

    assert_eq!(
        sched.metrics.total_collisions, 0,
        "different-SF interferer TXs must not collide"
    );

    let observed = observed.borrow();
    for label in ["i1", "i2"] {
        let started = observed
            .iter()
            .filter(|e| e.label == label && e.kind == "started")
            .count();
        assert_eq!(
            started, 2,
            "{label} must observe both starts even when SFs are orthogonal (got {started})"
        );
    }
}
