//! Scheduler ordering and termination invariants.
//!
//! Locks in three load-bearing guarantees of `theatron::scheduler::Scheduler`
//! that were only indirectly exercised before:
//!
//! 1. FIFO among simultaneous events (the `seq` tiebreaker in
//!    `ScheduledEvent::cmp`) — the foundation of the project's determinism
//!    promise. `BinaryHeap`'s internal tiebreaking is not guaranteed by std,
//!    so without the tiebreaker `Scheduler::run` would be order-dependent.
//!
//! 2. `end_time` boundary inclusivity — the run loop breaks on
//!    `event.time > end_time`, so a wake at exactly `end_time` must fire
//!    while a wake one tick past must not.
//!
//! 3. Broadcast receiver-visit order stability — `deliver_completed_to_nodes`
//!    iterates `self.nodes` in registration order; if someone ever swaps the
//!    `Vec` for a `HashMap`, deterministic replays would silently break.

use std::cell::RefCell;
use std::rc::Rc;

use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

fn tx(payload: Vec<u8>) -> Transmission {
    Transmission {
        payload,
        sf: 7,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency: 868_100_000,
        duration_us: 50_000,
        tx_power_dbm: 14,
    }
}

/// Records its wake time and receive order into shared logs.
struct LoggingNode {
    id: NodeId,
    log: Rc<RefCell<Vec<(NodeId, SimTime)>>>,
    pending_tx: Option<Transmission>,
    pending_rx: Rc<RefCell<Vec<NodeId>>>,
}

impl LoggingNode {
    fn new(
        id: u32,
        log: Rc<RefCell<Vec<(NodeId, SimTime)>>>,
        pending_tx: Option<Transmission>,
        pending_rx: Rc<RefCell<Vec<NodeId>>>,
    ) -> Self {
        Self {
            id: NodeId(id),
            log,
            pending_tx,
            pending_rx,
        }
    }
}

impl NodeHandle for LoggingNode {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        self.pending_rx.borrow_mut().push(self.id);
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        self.pending_tx.take()
    }
    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        self.log.borrow_mut().push((self.id, time));
        None
    }
}

// --- Invariant 1: FIFO among simultaneous wake events ---

/// Nodes registered with the same initial wake time fire their `update`
/// callbacks in registration order.
#[test]
fn simultaneous_wakes_fire_in_insertion_order() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::new(RefCell::new(Vec::new()));

    let mut sched = Scheduler::new(100_000);
    for id in [5u32, 1, 42, 7, 3] {
        sched.add_node(
            Box::new(LoggingNode::new(
                id,
                Rc::clone(&log),
                None,
                Rc::clone(&sink),
            )),
            Some(0),
        );
    }
    sched.run();

    let observed: Vec<NodeId> = log.borrow().iter().map(|(id, _)| *id).collect();
    assert_eq!(
        observed,
        vec![NodeId(5), NodeId(1), NodeId(42), NodeId(7), NodeId(3)],
        "simultaneous wakes must fire in registration order for determinism",
    );
    assert!(log.borrow().iter().all(|(_, t)| *t == 0));
}

/// Two runs of the same scenario produce identical event sequences.
#[test]
fn run_is_bitwise_deterministic_across_runs() {
    fn run_once() -> Vec<(NodeId, SimTime)> {
        let log = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::new(RefCell::new(Vec::new()));
        let mut sched = Scheduler::new(500_000);
        for id in [10u32, 20, 30] {
            sched.add_node(
                Box::new(LoggingNode::new(
                    id,
                    Rc::clone(&log),
                    None,
                    Rc::clone(&sink),
                )),
                Some(100_000),
            );
        }
        sched.run();
        log.borrow().clone()
    }

    assert_eq!(
        run_once(),
        run_once(),
        "scheduler must be deterministic run-to-run",
    );
}

// --- Invariant 2: end_time boundary is inclusive ---

/// A wake at exactly `end_time` must still be dispatched.
#[test]
fn wake_at_exact_end_time_fires() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::new(RefCell::new(Vec::new()));
    const END: SimTime = 250_000;

    let mut sched = Scheduler::new(END);
    sched.add_node(
        Box::new(LoggingNode::new(1, Rc::clone(&log), None, Rc::clone(&sink))),
        Some(END),
    );
    sched.run();

    assert_eq!(
        *log.borrow(),
        vec![(NodeId(1), END)],
        "wake at exactly end_time must fire (loop breaks on `>`, not `>=`)",
    );
}

/// A wake one microsecond past `end_time` must not be dispatched.
#[test]
fn wake_past_end_time_is_dropped() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::new(RefCell::new(Vec::new()));
    const END: SimTime = 250_000;

    let mut sched = Scheduler::new(END);
    sched.add_node(
        Box::new(LoggingNode::new(1, Rc::clone(&log), None, Rc::clone(&sink))),
        Some(END + 1),
    );
    sched.run();

    assert!(
        log.borrow().is_empty(),
        "wake past end_time must never be dispatched, got {:?}",
        log.borrow(),
    );
    assert_eq!(
        sched.current_time(),
        0,
        "scheduler must not advance time when the only event is past end_time",
    );
}

/// An interferer whose first poll is past `end_time` must not inject.
#[test]
fn interferer_first_poll_past_end_time_is_dropped() {
    struct TrackingInterferer {
        injected: Rc<RefCell<u32>>,
    }
    impl InterferenceSource for TrackingInterferer {
        fn observe(&mut self, _: &ChannelEvent, _: SimTime) {}
        fn poll_inject(&mut self, _: SimTime) -> Option<Transmission> {
            *self.injected.borrow_mut() += 1;
            None
        }
        fn next_poll_time(&self, _: SimTime) -> Option<SimTime> {
            None
        }
    }

    let injected = Rc::new(RefCell::new(0u32));
    let mut sched = Scheduler::new(100_000);
    sched.add_interferer(
        Box::new(TrackingInterferer {
            injected: Rc::clone(&injected),
        }),
        100_001,
    );
    sched.run();

    assert_eq!(
        *injected.borrow(),
        0,
        "interferer past end_time must not be polled",
    );
}

// --- Invariant 3: broadcast receiver-visit order is stable ---

/// A single TX delivered to multiple receivers visits them in
/// registration order.
#[test]
fn broadcast_delivers_in_registration_order() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let rx_order = Rc::new(RefCell::new(Vec::new()));

    let mut sched = Scheduler::new(200_000);

    sched.add_node(
        Box::new(LoggingNode::new(
            0,
            Rc::clone(&log),
            Some(tx(vec![0xAA])),
            Rc::clone(&rx_order),
        )),
        Some(0),
    );

    for id in [50u32, 3, 17, 99, 2] {
        sched.add_node(
            Box::new(LoggingNode::new(
                id,
                Rc::clone(&log),
                None,
                Rc::clone(&rx_order),
            )),
            None,
        );
    }
    sched.run();

    assert_eq!(
        *rx_order.borrow(),
        vec![NodeId(50), NodeId(3), NodeId(17), NodeId(99), NodeId(2)],
        "broadcast must visit receivers in registration order",
    );
    assert_eq!(sched.metrics.total_rx, 5);
}

// --- Invariant 4: self-rescheduling stops at end_time ---

/// A periodic node stops firing exactly at `end_time`.
#[test]
fn periodic_wake_stops_at_end_time_boundary() {
    struct PeriodicLogger {
        id: NodeId,
        period: SimTime,
        log: Rc<RefCell<Vec<SimTime>>>,
    }
    impl NodeHandle for PeriodicLogger {
        fn node_id(&self) -> NodeId {
            self.id
        }
        fn on_receive(&mut self, _: RxMetadata, _: SimTime) -> Option<SimTime> {
            None
        }
        fn poll_transmit(&mut self, _: SimTime) -> Option<Transmission> {
            None
        }
        fn update(&mut self, t: SimTime) -> Option<SimTime> {
            self.log.borrow_mut().push(t);
            Some(t + self.period)
        }
    }

    const END: SimTime = 300_000;
    const PERIOD: SimTime = 100_000;
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sched = Scheduler::new(END);
    sched.add_node(
        Box::new(PeriodicLogger {
            id: NodeId(1),
            period: PERIOD,
            log: Rc::clone(&log),
        }),
        Some(0),
    );
    sched.run();

    assert_eq!(
        *log.borrow(),
        vec![0, 100_000, 200_000, 300_000],
        "periodic wakes must fire at every multiple <= end_time and stop past it",
    );
}
