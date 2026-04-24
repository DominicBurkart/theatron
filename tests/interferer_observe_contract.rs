//! Contract tests for `InterferenceSource::observe` dispatch by the scheduler.
//!
//! # What is the observe contract?
//!
//! Every `InterferenceSource` registered with the [`Scheduler`] must be handed
//! every [`ChannelEvent`] produced by the channel it shares with the simulation:
//!
//! * A [`ChannelEvent::TransmissionStarted`] event must be delivered to every
//!   registered interferer when a transmission begins, whether the sender is a
//!   node or another interferer.
//! * A [`ChannelEvent::TransmissionCompleted`] event must be delivered to every
//!   registered interferer when a transmission completes.
//! * The number of `TransmissionStarted` events observed must equal the number
//!   of `TransmissionCompleted` events observed over the life of a simulation
//!   that runs to quiescence. (Outstanding active transmissions that never
//!   complete before end-time are the only exception; the tests here ensure
//!   every TX completes before end-time.)
//! * The `time` field of each observed `TransmissionStarted` must match the
//!   simulation time at which the scheduler dispatched the observation, and
//!   likewise for `TransmissionCompleted`.
//!
//! # Why does this matter?
//!
//! `InterferenceSource::observe` is the sole channel by which interferers (e.g.
//! adaptive jammers, carrier-sense interferers, passive traffic monitors) learn
//! what is happening on the shared medium. If the scheduler ever silently drops
//! an event — for example by only notifying the interferer that caused an event,
//! or by skipping `TransmissionCompleted` for interferer-originated TXs — entire
//! classes of interference models would fail subtly and produce wrong results.
//!
//! Prior to this test file, `InterferenceSource::observe` had zero coverage of
//! the dispatch path: no existing test inspects what the scheduler passes to an
//! interferer. Every registered implementation simply `_`-ignored the argument.

use std::cell::RefCell;
use std::rc::Rc;

use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// A record of a single observed channel event.
#[derive(Debug, Clone)]
struct Observation {
    event: ChannelEvent,
    time: SimTime,
}

/// An interferer that records every event it observes via a shared `RefCell`
/// so tests can inspect dispatch after the simulation runs.
///
/// This interferer never injects transmissions of its own — its sole purpose
/// is to witness what the scheduler reports to it.
struct ObservingInterferer {
    log: Rc<RefCell<Vec<Observation>>>,
}

impl InterferenceSource for ObservingInterferer {
    fn observe(&mut self, event: &ChannelEvent, time: SimTime) {
        self.log.borrow_mut().push(Observation {
            event: event.clone(),
            time,
        });
    }
    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        None
    }
    fn next_poll_time(&self, _current_time: SimTime) -> Option<SimTime> {
        None
    }
}

/// An interferer that injects a fixed number of transmissions and also logs
/// every event it observes, so tests can verify an interferer observes its
/// own transmissions' start and completion.
struct LoggingActiveInterferer {
    log: Rc<RefCell<Vec<Observation>>>,
    tx: Transmission,
    remaining: usize,
    interval: u64,
}

impl InterferenceSource for LoggingActiveInterferer {
    fn observe(&mut self, event: &ChannelEvent, time: SimTime) {
        self.log.borrow_mut().push(Observation {
            event: event.clone(),
            time,
        });
    }
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

/// A trivial node that queues a single transmission and then goes silent.
struct OneShotNode {
    id: NodeId,
    pending: Option<Transmission>,
}

impl OneShotNode {
    fn new(id: u32, tx: Transmission) -> Self {
        Self {
            id: NodeId(id),
            pending: Some(tx),
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
    fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> {
        self.pending.take()
    }
    fn update(&mut self, _t: SimTime) -> Option<SimTime> {
        None
    }
}

// Count `TransmissionStarted` events from a given sender in a log.
fn count_started(log: &[Observation], sender_id: NodeId) -> usize {
    log.iter()
        .filter(|o| {
            matches!(
                &o.event,
                ChannelEvent::TransmissionStarted { sender, .. } if *sender == sender_id
            )
        })
        .count()
}

// Count `TransmissionCompleted` events from a given sender in a log.
fn count_completed(log: &[Observation], sender_id: NodeId) -> usize {
    log.iter()
        .filter(|o| {
            matches!(
                &o.event,
                ChannelEvent::TransmissionCompleted { sender, .. } if *sender == sender_id
            )
        })
        .count()
}

// ---------------------------------------------------------------------------
// Tests: a single passive interferer observes a single node TX
// ---------------------------------------------------------------------------

/// When a single node transmits once, the registered passive interferer must
/// observe both the `TransmissionStarted` and `TransmissionCompleted` events
/// with correct sender, timing, and ordering.
#[test]
fn passive_interferer_observes_node_tx_start_and_complete() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let interferer = ObservingInterferer {
        log: Rc::clone(&log),
    };

    let tx = make_tx(7, 868_100_000, 50_000);
    let node = OneShotNode::new(1, tx);

    let mut sched = Scheduler::new(200_000);
    sched.add_interferer(Box::new(interferer), 0);
    sched.add_node(Box::new(node), Some(0));
    sched.run();

    let log = log.borrow();
    assert_eq!(
        log.len(),
        2,
        "interferer must see exactly one Started + one Completed, saw {:?}",
        *log
    );

    // First event: Started
    match &log[0].event {
        ChannelEvent::TransmissionStarted {
            sender,
            sf,
            frequency,
            time,
        } => {
            assert_eq!(*sender, NodeId(1));
            assert_eq!(*sf, 7);
            assert_eq!(*frequency, 868_100_000);
            assert_eq!(*time, 0, "Started.time field must be transmission start");
            assert_eq!(log[0].time, 0, "observe was called at t=0");
        }
        other => panic!("expected TransmissionStarted first, got {other:?}"),
    }

    // Second event: Completed
    match &log[1].event {
        ChannelEvent::TransmissionCompleted {
            sender,
            time,
            collided,
        } => {
            assert_eq!(*sender, NodeId(1));
            assert_eq!(*time, 50_000);
            assert!(
                !collided,
                "single TX must not be marked collided: {:?}",
                log[1]
            );
            assert_eq!(log[1].time, 50_000, "observe was called at completion time");
        }
        other => panic!("expected TransmissionCompleted second, got {other:?}"),
    }
}

/// The `time` field on each observed event must match the `time` argument
/// passed to `observe` — i.e. the scheduler does not drift between its own
/// clock and the event payload.
#[test]
fn observe_event_time_matches_dispatch_time() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let interferer = ObservingInterferer {
        log: Rc::clone(&log),
    };

    let tx = make_tx(7, 868_100_000, 37_500);
    let node = OneShotNode::new(1, tx);

    let mut sched = Scheduler::new(200_000);
    sched.add_interferer(Box::new(interferer), 0);
    sched.add_node(Box::new(node), Some(10_000));
    sched.run();

    let log = log.borrow();
    for obs in log.iter() {
        let event_time = match &obs.event {
            ChannelEvent::TransmissionStarted { time, .. } => *time,
            ChannelEvent::TransmissionCompleted { time, .. } => *time,
        };
        assert_eq!(
            event_time, obs.time,
            "event payload time must match the observe() dispatch time; got {obs:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests: started-count == completed-count invariant
// ---------------------------------------------------------------------------

/// Over a run in which every transmission completes before `end_time`, the
/// number of `TransmissionStarted` events observed must equal the number of
/// `TransmissionCompleted` events observed, per sender.
#[test]
fn started_and_completed_counts_match_per_sender() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let interferer = ObservingInterferer {
        log: Rc::clone(&log),
    };

    let mut sched = Scheduler::new(500_000);
    sched.add_interferer(Box::new(interferer), 0);

    // Three non-overlapping node TXs (staggered start / same SF+freq but
    // separated in time so they do not collide).
    sched.add_node(
        Box::new(OneShotNode::new(1, make_tx(7, 868_100_000, 50_000))),
        Some(0),
    );
    sched.add_node(
        Box::new(OneShotNode::new(2, make_tx(7, 868_100_000, 50_000))),
        Some(100_000),
    );
    sched.add_node(
        Box::new(OneShotNode::new(3, make_tx(7, 868_100_000, 50_000))),
        Some(200_000),
    );
    sched.run();

    let log = log.borrow();
    for sender in [NodeId(1), NodeId(2), NodeId(3)] {
        let started = count_started(&log, sender);
        let completed = count_completed(&log, sender);
        assert_eq!(
            started, 1,
            "sender {sender:?} must have exactly 1 Started, saw {started}",
        );
        assert_eq!(
            completed, 1,
            "sender {sender:?} must have exactly 1 Completed, saw {completed}",
        );
    }
    // Total: 3 starts + 3 completes = 6 events.
    assert_eq!(log.len(), 6, "full event log = {log:?}");
}

// ---------------------------------------------------------------------------
// Tests: every registered interferer is notified (broadcast semantics)
// ---------------------------------------------------------------------------

/// Every registered interferer — not just the first one — must receive every
/// channel event. This guards against regressions that would notify only a
/// single interferer instance.
#[test]
fn all_registered_interferers_receive_same_events() {
    let log_a = Rc::new(RefCell::new(Vec::new()));
    let log_b = Rc::new(RefCell::new(Vec::new()));
    let log_c = Rc::new(RefCell::new(Vec::new()));

    let mut sched = Scheduler::new(200_000);
    sched.add_interferer(
        Box::new(ObservingInterferer {
            log: Rc::clone(&log_a),
        }),
        0,
    );
    sched.add_interferer(
        Box::new(ObservingInterferer {
            log: Rc::clone(&log_b),
        }),
        0,
    );
    sched.add_interferer(
        Box::new(ObservingInterferer {
            log: Rc::clone(&log_c),
        }),
        0,
    );

    sched.add_node(
        Box::new(OneShotNode::new(1, make_tx(7, 868_100_000, 50_000))),
        Some(0),
    );
    sched.run();

    let a = log_a.borrow();
    let b = log_b.borrow();
    let c = log_c.borrow();
    assert_eq!(a.len(), 2, "log_a = {a:?}");
    assert_eq!(b.len(), 2, "log_b = {b:?}");
    assert_eq!(c.len(), 2, "log_c = {c:?}");

    // Each interferer saw the same per-event timing.
    for i in 0..2 {
        assert_eq!(a[i].time, b[i].time);
        assert_eq!(b[i].time, c[i].time);
    }
}

// ---------------------------------------------------------------------------
// Tests: an interferer observes its own injected transmissions
// ---------------------------------------------------------------------------

/// When an interferer injects a transmission via `poll_inject`, it must
/// subsequently observe both the `TransmissionStarted` for its own TX *and* the
/// matching `TransmissionCompleted`. This is the cross-dispatch invariant:
/// injection and observation are symmetric across all registered interferers,
/// including the source.
#[test]
fn interferer_observes_own_injected_tx_start_and_complete() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let tx = make_tx(7, 868_100_000, 40_000);
    let interferer = LoggingActiveInterferer {
        log: Rc::clone(&log),
        tx: tx.clone(),
        remaining: 1,
        interval: 100_000,
    };

    let mut sched = Scheduler::new(200_000);
    sched.add_interferer(Box::new(interferer), 0);
    sched.run();

    let log = log.borrow();
    // The interferer is registered as synthetic NodeId(u32::MAX).
    let synthetic = NodeId(u32::MAX);
    let started = count_started(&log, synthetic);
    let completed = count_completed(&log, synthetic);
    assert_eq!(
        started, 1,
        "interferer must observe its own Started event, log={log:?}",
    );
    assert_eq!(
        completed, 1,
        "interferer must observe its own Completed event, log={log:?}",
    );
}

/// When two interferers are registered and one of them injects, the *other*
/// interferer must also observe the injected transmission's start and
/// completion. This guards against a regression that notifies only the
/// injecting interferer.
#[test]
fn sibling_interferer_observes_peer_injected_tx() {
    let inj_log = Rc::new(RefCell::new(Vec::new()));
    let peer_log = Rc::new(RefCell::new(Vec::new()));
    let tx = make_tx(7, 868_100_000, 40_000);

    let injecting = LoggingActiveInterferer {
        log: Rc::clone(&inj_log),
        tx: tx.clone(),
        remaining: 1,
        interval: 100_000,
    };
    let peer = ObservingInterferer {
        log: Rc::clone(&peer_log),
    };

    let mut sched = Scheduler::new(200_000);
    sched.add_interferer(Box::new(injecting), 0);
    sched.add_interferer(Box::new(peer), 0);
    sched.run();

    let peer_log = peer_log.borrow();
    // Second interferer is synthetic NodeId(u32::MAX - 1); the first is
    // u32::MAX. Only the first interferer injects.
    let source = NodeId(u32::MAX);
    let peer_started = count_started(&peer_log, source);
    let peer_completed = count_completed(&peer_log, source);
    assert_eq!(
        peer_started, 1,
        "peer interferer must observe sibling's Started, log={peer_log:?}",
    );
    assert_eq!(
        peer_completed, 1,
        "peer interferer must observe sibling's Completed, log={peer_log:?}",
    );
}

// ---------------------------------------------------------------------------
// Tests: ordering — Started precedes Completed for the same TX
// ---------------------------------------------------------------------------

/// For every sender, the observed `TransmissionStarted` must strictly precede
/// the observed `TransmissionCompleted` in the log, and the gap must equal
/// the transmission's duration.
#[test]
fn started_precedes_completed_for_same_sender() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let interferer = ObservingInterferer {
        log: Rc::clone(&log),
    };

    let mut sched = Scheduler::new(500_000);
    sched.add_interferer(Box::new(interferer), 0);

    let tx1 = make_tx(7, 868_100_000, 50_000);
    let tx2 = make_tx(8, 868_300_000, 80_000);
    sched.add_node(Box::new(OneShotNode::new(1, tx1)), Some(0));
    sched.add_node(Box::new(OneShotNode::new(2, tx2)), Some(0));
    sched.run();

    let log = log.borrow();
    for sender in [NodeId(1), NodeId(2)] {
        let start_idx = log
            .iter()
            .position(|o| {
                matches!(
                    &o.event,
                    ChannelEvent::TransmissionStarted { sender: s, .. } if *s == sender
                )
            })
            .expect("Started event present");
        let complete_idx = log
            .iter()
            .position(|o| {
                matches!(
                    &o.event,
                    ChannelEvent::TransmissionCompleted { sender: s, .. } if *s == sender
                )
            })
            .expect("Completed event present");
        assert!(
            start_idx < complete_idx,
            "for {sender:?} Started (idx {start_idx}) must precede Completed (idx {complete_idx}), log={log:?}",
        );
        // Gap must equal the tx duration (50_000 for N1, 80_000 for N2).
        let expected_duration = if sender == NodeId(1) { 50_000 } else { 80_000 };
        assert_eq!(
            log[complete_idx].time - log[start_idx].time,
            expected_duration,
            "Completed time - Started time must equal duration for {sender:?}",
        );
    }
}

// ---------------------------------------------------------------------------
// Tests: collided transmissions still emit Started + Completed
// ---------------------------------------------------------------------------

/// A collision does not suppress event observation. Even when two TXs collide,
/// the interferer must still observe a `TransmissionStarted` and a
/// `TransmissionCompleted` (with `collided: true`) for each.
#[test]
fn collided_tx_still_emits_started_and_completed() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let interferer = ObservingInterferer {
        log: Rc::clone(&log),
    };

    let mut sched = Scheduler::new(200_000);
    sched.add_interferer(Box::new(interferer), 0);

    let tx1 = make_tx(7, 868_100_000, 50_000);
    let tx2 = make_tx(7, 868_100_000, 50_000);
    sched.add_node(Box::new(OneShotNode::new(1, tx1)), Some(0));
    sched.add_node(Box::new(OneShotNode::new(2, tx2)), Some(10_000));
    sched.run();

    let log = log.borrow();
    for sender in [NodeId(1), NodeId(2)] {
        let started = count_started(&log, sender);
        let completed = count_completed(&log, sender);
        assert_eq!(
            started, 1,
            "collided TX from {sender:?} still emits Started"
        );
        assert_eq!(
            completed, 1,
            "collided TX from {sender:?} still emits Completed"
        );
    }
    // The two Completed events must carry collided=true
    let collided_count = log
        .iter()
        .filter(|o| {
            matches!(
                &o.event,
                ChannelEvent::TransmissionCompleted { collided: true, .. }
            )
        })
        .count();
    assert_eq!(
        collided_count, 2,
        "both Completed events must have collided=true"
    );
}
