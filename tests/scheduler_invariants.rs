//! Invariant tests for the scheduler-channel-metrics interaction.
//!
//! These tests pin down behaviour that the existing suite implicitly relies on
//! but does not explicitly assert. Each section documents the invariant,
//! sourced from the implementation in `src/scheduler.rs`:
//!
//! 1. A sender never receives its own transmission (the `if self.nodes[i].node_id() != sender`
//!    guard in `deliver_completed_to_nodes`).
//! 2. Collided frames are *not* delivered: `total_rx` only counts non-collided receptions
//!    (the `if collided { record_collision } else { ... }` branch).
//! 3. `total_rx` equals the sum of per-node receive counts (consistency between aggregate
//!    and per-node `MetricsCollector` counters).
//! 4. Cross-observation between interferers: when one interferer injects a frame, *all*
//!    interferers — including itself — receive both the `TransmissionStarted` and
//!    `TransmissionCompleted` channel events (the `for i in 0..self.interferers.len()`
//!    loops in `handle_poll_transmit` analogues for the interference branch).
//! 5. Same-time FIFO ordering: events scheduled at the same `SimTime` fire in insertion
//!    order, because `ScheduledEvent::cmp` ties on `seq` (this is what makes the
//!    simulation deterministic across reruns of identical scenarios).

use std::cell::RefCell;
use std::rc::Rc;

use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// (time, node_id, kind) — node-side event log.
type NodeLog = Rc<RefCell<Vec<(SimTime, u32, &'static str)>>>;
/// (time, label, kind, sender) — interferer-side event log.
type InterfererLog = Rc<RefCell<Vec<(SimTime, &'static str, &'static str, NodeId)>>>;
/// (time, node_id) — wake-only log.
type WakeLog = Rc<RefCell<Vec<(SimTime, u32)>>>;

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

/// A node that can be configured to transmit once and to record an ordered log
/// of `update`/`on_receive`/`poll_transmit` calls keyed by simulation time.
struct LoggingNode {
    id: NodeId,
    pending_tx: Option<Transmission>,
    /// Shared event log: (time, node_id, kind).
    log: NodeLog,
}

impl LoggingNode {
    fn new(id: u32, log: NodeLog, pending_tx: Option<Transmission>) -> Self {
        Self {
            id: NodeId(id),
            pending_tx,
            log,
        }
    }
}

impl NodeHandle for LoggingNode {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _frame: RxMetadata, time: SimTime) -> Option<SimTime> {
        self.log.borrow_mut().push((time, self.id.0, "rx"));
        None
    }
    fn poll_transmit(&mut self, time: SimTime) -> Option<Transmission> {
        let next = self.pending_tx.take();
        if next.is_some() {
            self.log.borrow_mut().push((time, self.id.0, "tx"));
        }
        next
    }
    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        self.log.borrow_mut().push((time, self.id.0, "update"));
        None
    }
}

/// An interferer that records every channel event it observes.
struct ObservingInterferer {
    label: &'static str,
    log: InterfererLog,
    inject_once: Option<Transmission>,
    first_poll_done: bool,
}

impl ObservingInterferer {
    fn new(label: &'static str, log: InterfererLog, inject_once: Option<Transmission>) -> Self {
        Self {
            label,
            log,
            inject_once,
            first_poll_done: false,
        }
    }
}

impl InterferenceSource for ObservingInterferer {
    fn observe(&mut self, event: &ChannelEvent, time: SimTime) {
        let (kind, sender) = match event {
            ChannelEvent::TransmissionStarted { sender, .. } => ("started", *sender),
            ChannelEvent::TransmissionCompleted { sender, .. } => ("completed", *sender),
        };
        self.log.borrow_mut().push((time, self.label, kind, sender));
    }
    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        if self.first_poll_done {
            return None;
        }
        self.first_poll_done = true;
        self.inject_once.take()
    }
    fn next_poll_time(&self, _current_time: SimTime) -> Option<SimTime> {
        None
    }
}

/// Counts wakes and records the time of each.
struct WakeCounter {
    id: NodeId,
    log: WakeLog,
}

impl NodeHandle for WakeCounter {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        None
    }
    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        self.log.borrow_mut().push((time, self.id.0));
        None
    }
}

// ---------------------------------------------------------------------------
// Invariant 1: sender never receives its own transmission.
// ---------------------------------------------------------------------------

#[test]
fn sender_never_receives_own_broadcast() {
    let mut sched = Scheduler::new(200_000);
    let log: NodeLog = Rc::new(RefCell::new(Vec::new()));
    let sender = LoggingNode::new(1, log.clone(), Some(tx(7, 868_100_000, 50_000, 14)));
    sched.add_node(Box::new(sender), Some(0));
    sched.add_node(Box::new(LoggingNode::new(2, log.clone(), None)), None);
    sched.add_node(Box::new(LoggingNode::new(3, log.clone(), None)), None);
    sched.run();

    // Sender never received anything; receivers each got exactly one frame.
    assert_eq!(
        sched.metrics.node_rx_count(NodeId(1)),
        0,
        "sender must not receive its own transmission"
    );
    assert_eq!(sched.metrics.node_rx_count(NodeId(2)), 1);
    assert_eq!(sched.metrics.node_rx_count(NodeId(3)), 1);

    // No "rx" entry should be present for node 1 in the log.
    let log = log.borrow();
    assert!(
        log.iter().all(|(_, id, kind)| !(*id == 1 && *kind == "rx")),
        "log should contain no rx entry for the sender; got: {:?}",
        log
    );
}

// ---------------------------------------------------------------------------
// Invariant 2: collided frames are not delivered.
// ---------------------------------------------------------------------------

#[test]
fn collided_frames_are_not_delivered() {
    let mut sched = Scheduler::new(200_000);
    let log = Rc::new(RefCell::new(Vec::new()));

    sched.add_node(
        Box::new(LoggingNode::new(
            1,
            log.clone(),
            Some(tx(7, 868_100_000, 50_000, 14)),
        )),
        Some(0),
    );
    sched.add_node(
        Box::new(LoggingNode::new(
            2,
            log.clone(),
            Some(tx(7, 868_100_000, 50_000, 14)),
        )),
        Some(10_000),
    );
    // A passive receiver: should observe nothing because both senders collide.
    sched.add_node(Box::new(LoggingNode::new(3, log.clone(), None)), None);
    sched.run();

    assert_eq!(sched.metrics.total_tx, 2);
    assert_eq!(sched.metrics.total_collisions, 2);
    assert_eq!(
        sched.metrics.total_rx, 0,
        "collided frames must not be counted as receptions"
    );
    let log = log.borrow();
    assert!(
        log.iter().all(|(_, _, kind)| *kind != "rx"),
        "no node's on_receive should fire for collided frames; got: {:?}",
        log
    );
}

// ---------------------------------------------------------------------------
// Invariant 3: aggregate total_rx == sum of per-node rx counts.
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn aggregate_total_rx_equals_sum_of_per_node(n_receivers in 1usize..10) {
        let mut sched = Scheduler::new(200_000);
        sched.add_node(
            Box::new(LoggingNode::new(
                0,
                Rc::new(RefCell::new(Vec::new())),
                Some(tx(7, 868_100_000, 50_000, 14)),
            )),
            Some(0),
        );
        for i in 1..=n_receivers {
            sched.add_node(
                Box::new(LoggingNode::new(
                    i as u32,
                    Rc::new(RefCell::new(Vec::new())),
                    None,
                )),
                None,
            );
        }
        sched.run();

        let mut sum = 0u64;
        for i in 0..=n_receivers {
            sum += sched.metrics.node_rx_count(NodeId(i as u32));
        }
        prop_assert_eq!(sched.metrics.total_rx, sum);
        prop_assert_eq!(sched.metrics.total_rx, n_receivers as u64);
    }
}

// ---------------------------------------------------------------------------
// Invariant 4: all interferers observe channel events for any interferer-injected
// transmission (started + completed), not just node-originated ones. This is the
// behaviour relied upon by passive eavesdroppers and adversarial replay models
// from ARCHITECTURE.md.
// ---------------------------------------------------------------------------

#[test]
fn all_interferers_observe_interferer_originated_events() {
    let mut sched = Scheduler::new(300_000);
    let log: InterfererLog = Rc::new(RefCell::new(Vec::new()));

    // Active interferer that injects exactly one transmission at t=0.
    let active =
        ObservingInterferer::new("active", log.clone(), Some(tx(7, 868_100_000, 50_000, 14)));
    // Passive interferer that only observes.
    let passive = ObservingInterferer::new("passive", log.clone(), None);

    sched.add_interferer(Box::new(active), 0);
    sched.add_interferer(Box::new(passive), 0);
    sched.run();

    // Each interferer should see one "started" and one "completed" for the
    // interferer-originated TX. Synthetic interferer NodeIds occupy the top of
    // the u32 range (u32::MAX, u32::MAX-1).
    let log = log.borrow();
    let active_started = log
        .iter()
        .filter(|(_, l, k, _)| *l == "active" && *k == "started")
        .count();
    let passive_started = log
        .iter()
        .filter(|(_, l, k, _)| *l == "passive" && *k == "started")
        .count();
    let active_completed = log
        .iter()
        .filter(|(_, l, k, _)| *l == "active" && *k == "completed")
        .count();
    let passive_completed = log
        .iter()
        .filter(|(_, l, k, _)| *l == "passive" && *k == "completed")
        .count();

    assert_eq!(
        active_started, 1,
        "active interferer must observe its own started event"
    );
    assert_eq!(
        passive_started, 1,
        "passive interferer must observe other interferers' started events"
    );
    assert_eq!(active_completed, 1);
    assert_eq!(passive_completed, 1);
}

#[test]
fn passive_interferer_observes_node_transmissions() {
    let mut sched = Scheduler::new(200_000);
    let log: InterfererLog = Rc::new(RefCell::new(Vec::new()));
    let passive = ObservingInterferer::new("eavesdropper", log.clone(), None);
    sched.add_interferer(Box::new(passive), 0);

    let node_log = Rc::new(RefCell::new(Vec::new()));
    sched.add_node(
        Box::new(LoggingNode::new(
            1,
            node_log,
            Some(tx(7, 868_100_000, 50_000, 14)),
        )),
        Some(0),
    );
    sched.run();

    let log = log.borrow();
    let started: Vec<_> = log.iter().filter(|(_, _, k, _)| *k == "started").collect();
    let completed: Vec<_> = log
        .iter()
        .filter(|(_, _, k, _)| *k == "completed")
        .collect();
    assert_eq!(started.len(), 1, "passive must observe node TX start");
    assert_eq!(
        completed.len(),
        1,
        "passive must observe node TX completion"
    );
    assert_eq!(started[0].3, NodeId(1));
    assert_eq!(completed[0].3, NodeId(1));
}

// ---------------------------------------------------------------------------
// Invariant 5: same-time FIFO ordering for determinism.
//
// Two PeriodicNodes scheduled with `initial_wake = Some(0)` both fire at t=0.
// The seq tie-breaker in ScheduledEvent::cmp guarantees that the node added
// first wakes first — and that the order does not depend on HashMap or
// BinaryHeap internal randomization.
// ---------------------------------------------------------------------------

#[test]
fn same_time_events_fire_in_insertion_order() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sched = Scheduler::new(100);

    sched.add_node(
        Box::new(WakeCounter {
            id: NodeId(10),
            log: log.clone(),
        }),
        Some(0),
    );
    sched.add_node(
        Box::new(WakeCounter {
            id: NodeId(20),
            log: log.clone(),
        }),
        Some(0),
    );
    sched.add_node(
        Box::new(WakeCounter {
            id: NodeId(30),
            log: log.clone(),
        }),
        Some(0),
    );
    sched.run();

    let events = log.borrow().clone();
    assert_eq!(events.len(), 3);
    // Each fires at t=0, in the order they were added.
    assert_eq!(events[0], (0, 10));
    assert_eq!(events[1], (0, 20));
    assert_eq!(events[2], (0, 30));
}

#[test]
fn rerunning_identical_scenario_produces_identical_metrics() {
    fn run_once() -> (u64, u64, u64, u64, u64) {
        let mut sched = Scheduler::new(500_000);
        // Two senders that will collide, plus three passive listeners.
        sched.add_node(
            Box::new(LoggingNode::new(
                1,
                Rc::new(RefCell::new(Vec::new())),
                Some(tx(7, 868_100_000, 50_000, 20)),
            )),
            Some(0),
        );
        sched.add_node(
            Box::new(LoggingNode::new(
                2,
                Rc::new(RefCell::new(Vec::new())),
                Some(tx(7, 868_100_000, 50_000, 14)),
            )),
            Some(10_000),
        );
        for i in 3..=5 {
            sched.add_node(
                Box::new(LoggingNode::new(i, Rc::new(RefCell::new(Vec::new())), None)),
                None,
            );
        }
        sched.run();
        (
            sched.metrics.total_tx,
            sched.metrics.total_rx,
            sched.metrics.total_collisions,
            sched.metrics.total_captures,
            sched.metrics.total_airtime_us,
        )
    }
    let a = run_once();
    let b = run_once();
    let c = run_once();
    assert_eq!(a, b, "scheduler must be deterministic across reruns");
    assert_eq!(b, c);
}
