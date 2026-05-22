//! Contract tests for [`InterferenceSource::observe`] integration with the scheduler.
//!
//! The scheduler is the only path through which an [`InterferenceSource`]
//! becomes aware of channel activity. Adaptive interferers (smart jammers,
//! CSMA-style sources, energy-detection models) rely on `observe` being called
//! at the right times with the right events. The contract verified here is:
//!
//! 1. Every node transmission produces a `TransmissionStarted` and a
//!    `TransmissionCompleted` event delivered to *every* registered
//!    [`InterferenceSource`] (including the one that injected the TX).
//! 2. An interferer that injects a transmission also receives the resulting
//!    `TransmissionStarted` event for its own injection (self-observation).
//! 3. `TransmissionCompleted` events carry the correct `collided` flag so an
//!    adaptive source can distinguish successful TXs from collided ones.
//! 4. Returning `None` from [`InterferenceSource::next_poll_time`] permanently
//!    stops further `poll_inject` calls (no spurious re-scheduling).
//! 5. The order of observed events is consistent with simulation time — events
//!    are observed in non-decreasing time order across the whole run.
//!
//! These invariants are not exercised by any existing test. A regression in
//! the scheduler's interferer-notification path could silently break every
//! adaptive interference model without any of the existing suites catching it.

use std::cell::RefCell;
use std::rc::Rc;

use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tx(sf: u8, frequency: u32, duration_us: u64) -> Transmission {
    Transmission {
        payload: vec![0xAA],
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency,
        duration_us,
        tx_power_dbm: 14,
    }
}

/// A node that transmits exactly one frame on the first wake.
struct OneShotNode {
    id: NodeId,
    pending: Option<Transmission>,
}

impl OneShotNode {
    fn new(id: u32, t: Transmission) -> Self {
        Self {
            id: NodeId(id),
            pending: Some(t),
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

/// An interferer that records every (event, observed_time) it is notified of
/// and counts how many times `poll_inject` is called.
///
/// `poll_inject` returns `None` always — i.e. it's a passive observer that
/// only cares about the `observe` callback. `next_poll_time` returns `None`
/// after `max_polls` has been reached so we can verify polling actually stops.
struct ObservingInterferer {
    observations: Rc<RefCell<Vec<(ChannelEvent, SimTime)>>>,
    poll_count: Rc<RefCell<u32>>,
    poll_interval: u64,
    max_polls: u32,
}

impl InterferenceSource for ObservingInterferer {
    fn observe(&mut self, event: &ChannelEvent, time: SimTime) {
        self.observations.borrow_mut().push((event.clone(), time));
    }

    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        *self.poll_count.borrow_mut() += 1;
        None
    }

    fn next_poll_time(&self, current_time: SimTime) -> Option<SimTime> {
        if *self.poll_count.borrow() < self.max_polls {
            Some(current_time + self.poll_interval)
        } else {
            None
        }
    }
}

/// An interferer that injects a single transmission on its first poll and
/// then goes silent. It also records every `observe` call so the test can
/// verify it sees its own injection.
struct InjectingObserver {
    inject: RefCell<Option<Transmission>>,
    observations: Rc<RefCell<Vec<(ChannelEvent, SimTime)>>>,
}

impl InterferenceSource for InjectingObserver {
    fn observe(&mut self, event: &ChannelEvent, time: SimTime) {
        self.observations.borrow_mut().push((event.clone(), time));
    }

    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        self.inject.borrow_mut().take()
    }

    fn next_poll_time(&self, _current_time: SimTime) -> Option<SimTime> {
        // Single-shot: never poll again.
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Every node transmission produces both a `TransmissionStarted` and a
/// `TransmissionCompleted` event delivered to a registered interferer.
///
/// Without this, an adaptive interferer cannot detect carrier nor know when
/// a TX has cleared — the very minimum it would need to back off, jam, or
/// model energy detection.
#[test]
fn interferer_observes_node_tx_start_and_completion() {
    let observations: Rc<RefCell<Vec<(ChannelEvent, SimTime)>>> = Rc::new(RefCell::new(Vec::new()));
    let poll_count = Rc::new(RefCell::new(0u32));

    let interferer = ObservingInterferer {
        observations: Rc::clone(&observations),
        poll_count: Rc::clone(&poll_count),
        poll_interval: 1_000_000,
        // One poll is enough; we just need the interferer registered so the
        // scheduler routes observe() calls to it.
        max_polls: 1,
    };

    let mut sched = Scheduler::new(500_000);
    sched.add_node(
        Box::new(OneShotNode::new(1, tx(7, 868_100_000, 50_000))),
        Some(0),
    );
    sched.add_interferer(Box::new(interferer), 0);
    sched.run();

    let obs = observations.borrow();

    let starts: Vec<_> = obs
        .iter()
        .filter(|(e, _)| matches!(e, ChannelEvent::TransmissionStarted { .. }))
        .collect();
    let completes: Vec<_> = obs
        .iter()
        .filter(|(e, _)| matches!(e, ChannelEvent::TransmissionCompleted { .. }))
        .collect();

    assert_eq!(
        starts.len(),
        1,
        "expected exactly one TransmissionStarted observation, got {}: {:?}",
        starts.len(),
        obs
    );
    assert_eq!(
        completes.len(),
        1,
        "expected exactly one TransmissionCompleted observation, got {}: {:?}",
        completes.len(),
        obs
    );

    // Verify the start event carries the correct sender/sf/freq/time.
    if let ChannelEvent::TransmissionStarted {
        sender,
        sf,
        frequency,
        time,
    } = &starts[0].0
    {
        assert_eq!(*sender, NodeId(1));
        assert_eq!(*sf, 7);
        assert_eq!(*frequency, 868_100_000);
        assert_eq!(*time, 0, "TX started at t=0");
    } else {
        unreachable!()
    }

    // Verify the completion event reports the correct end time and non-collision.
    if let ChannelEvent::TransmissionCompleted {
        sender,
        time,
        collided,
    } = &completes[0].0
    {
        assert_eq!(*sender, NodeId(1));
        assert_eq!(*time, 50_000, "TX completes at start + duration");
        assert!(!collided, "single TX should not be collided");
    } else {
        unreachable!()
    }
}

/// An interferer that injects its own transmission must also observe the
/// resulting `TransmissionStarted` event — this is the self-observation
/// invariant that any closed-loop interferer (e.g. one that backs off after
/// hearing its own carrier) depends on.
#[test]
fn interferer_observes_its_own_injected_tx() {
    let observations: Rc<RefCell<Vec<(ChannelEvent, SimTime)>>> = Rc::new(RefCell::new(Vec::new()));

    let injector = InjectingObserver {
        inject: RefCell::new(Some(tx(7, 868_100_000, 30_000))),
        observations: Rc::clone(&observations),
    };

    let mut sched = Scheduler::new(500_000);
    sched.add_interferer(Box::new(injector), 0);
    sched.run();

    let obs = observations.borrow();

    // The interferer should have seen its own start + completion.
    let starts: Vec<_> = obs
        .iter()
        .filter(|(e, _)| matches!(e, ChannelEvent::TransmissionStarted { .. }))
        .collect();
    let completes: Vec<_> = obs
        .iter()
        .filter(|(e, _)| matches!(e, ChannelEvent::TransmissionCompleted { .. }))
        .collect();

    assert_eq!(
        starts.len(),
        1,
        "interferer must observe its own TransmissionStarted event"
    );
    assert_eq!(
        completes.len(),
        1,
        "interferer must observe its own TransmissionCompleted event"
    );

    // The synthetic NodeId for an interferer is u32::MAX - idx (idx=0 here).
    if let ChannelEvent::TransmissionStarted { sender, .. } = &starts[0].0 {
        assert_eq!(
            *sender,
            NodeId(u32::MAX),
            "interferer sees its own injection under the synthetic interferer ID"
        );
    } else {
        unreachable!()
    }

    // The completion event must also carry the synthetic sender ID so that a
    // closed-loop interferer can correlate start and end for its own TX.
    if let ChannelEvent::TransmissionCompleted { sender, .. } = &completes[0].0 {
        assert_eq!(
            *sender,
            NodeId(u32::MAX),
            "TransmissionCompleted for self-injected TX must use the same synthetic interferer ID as TransmissionStarted"
        );
    } else {
        unreachable!()
    }
}

/// Two registered interferers must each observe events generated by the other.
/// This verifies the scheduler fans `observe()` out to *every* interferer, not
/// just the originator of the event.
#[test]
fn each_interferer_observes_other_interferers_tx() {
    let obs_a: Rc<RefCell<Vec<(ChannelEvent, SimTime)>>> = Rc::new(RefCell::new(Vec::new()));
    let obs_b: Rc<RefCell<Vec<(ChannelEvent, SimTime)>>> = Rc::new(RefCell::new(Vec::new()));

    // A injects a TX, B is silent. Both must see A's start + completion.
    let a = InjectingObserver {
        inject: RefCell::new(Some(tx(7, 868_100_000, 30_000))),
        observations: Rc::clone(&obs_a),
    };
    let b = InjectingObserver {
        inject: RefCell::new(None),
        observations: Rc::clone(&obs_b),
    };

    let mut sched = Scheduler::new(500_000);
    sched.add_interferer(Box::new(a), 0);
    sched.add_interferer(Box::new(b), 0);
    sched.run();

    for (label, observations) in [("A", &obs_a), ("B", &obs_b)] {
        let obs = observations.borrow();
        let starts = obs
            .iter()
            .filter(|(e, _)| matches!(e, ChannelEvent::TransmissionStarted { .. }))
            .count();
        let completes = obs
            .iter()
            .filter(|(e, _)| matches!(e, ChannelEvent::TransmissionCompleted { .. }))
            .count();
        assert_eq!(
            starts, 1,
            "interferer {label} must observe the other interferer's TransmissionStarted",
        );
        assert_eq!(
            completes, 1,
            "interferer {label} must observe the other interferer's TransmissionCompleted",
        );
    }
}

/// A `TransmissionCompleted` event observed by an interferer must carry the
/// correct `collided` flag so adaptive sources can distinguish successful TXs
/// from collisions.
#[test]
fn interferer_observes_collided_flag_on_completion() {
    let observations: Rc<RefCell<Vec<(ChannelEvent, SimTime)>>> = Rc::new(RefCell::new(Vec::new()));
    let poll_count = Rc::new(RefCell::new(0u32));

    let interferer = ObservingInterferer {
        observations: Rc::clone(&observations),
        poll_count: Rc::clone(&poll_count),
        poll_interval: 1_000_000,
        max_polls: 1,
    };

    let mut sched = Scheduler::new(500_000);
    // Two same-SF/freq nodes overlapping → both collide.
    sched.add_node(
        Box::new(OneShotNode::new(1, tx(7, 868_100_000, 50_000))),
        Some(0),
    );
    sched.add_node(
        Box::new(OneShotNode::new(2, tx(7, 868_100_000, 50_000))),
        Some(10_000),
    );
    sched.add_interferer(Box::new(interferer), 0);
    sched.run();

    let obs = observations.borrow();
    let completions: Vec<_> = obs
        .iter()
        .filter_map(|(e, _)| match e {
            ChannelEvent::TransmissionCompleted { collided, .. } => Some(*collided),
            _ => None,
        })
        .collect();

    assert_eq!(completions.len(), 2, "expected 2 completion events");
    assert!(
        completions.iter().all(|c| *c),
        "both overlapping same-SF/freq TXs must be reported as collided to the interferer: got {:?}",
        completions,
    );
}

/// Returning `None` from `next_poll_time` must permanently stop further
/// `poll_inject` calls. Without this guarantee, a one-shot interferer could
/// be polled forever and produce garbage state, or — worse — re-inject
/// transmissions it intended to suppress.
#[test]
fn next_poll_time_none_permanently_stops_polling() {
    let observations: Rc<RefCell<Vec<(ChannelEvent, SimTime)>>> = Rc::new(RefCell::new(Vec::new()));
    let poll_count = Rc::new(RefCell::new(0u32));

    let interferer = ObservingInterferer {
        observations: Rc::clone(&observations),
        poll_count: Rc::clone(&poll_count),
        poll_interval: 10_000,
        max_polls: 3, // After 3 polls, next_poll_time returns None.
    };

    let mut sched = Scheduler::new(10_000_000); // Way more than 3 * 10_000.
    sched.add_interferer(Box::new(interferer), 0);
    sched.run();

    assert_eq!(
        *poll_count.borrow(),
        3,
        "poll_inject must be called exactly max_polls times once next_poll_time returns None",
    );
}

/// The sequence of `(time, event)` observations delivered to an interferer
/// must be non-decreasing in time. This is the basic temporal-consistency
/// invariant adaptive interferers rely on for any time-windowed behavior
/// (rolling averages, duty-cycle accounting, exponential backoff).
#[test]
fn observed_event_times_are_non_decreasing() {
    let observations: Rc<RefCell<Vec<(ChannelEvent, SimTime)>>> = Rc::new(RefCell::new(Vec::new()));
    let poll_count = Rc::new(RefCell::new(0u32));

    let interferer = ObservingInterferer {
        observations: Rc::clone(&observations),
        poll_count: Rc::clone(&poll_count),
        poll_interval: 1_000_000,
        max_polls: 1,
    };

    // A burst of three sequential, non-overlapping TXs to generate six events.
    let mut sched = Scheduler::new(1_000_000);
    sched.add_node(
        Box::new(OneShotNode::new(1, tx(7, 868_100_000, 50_000))),
        Some(0),
    );
    sched.add_node(
        Box::new(OneShotNode::new(2, tx(7, 868_300_000, 50_000))),
        Some(100_000),
    );
    sched.add_node(
        Box::new(OneShotNode::new(3, tx(8, 868_100_000, 50_000))),
        Some(200_000),
    );
    sched.add_interferer(Box::new(interferer), 0);
    sched.run();

    let obs = observations.borrow();
    assert_eq!(obs.len(), 6, "3 starts + 3 completes = 6 observations");

    let times: Vec<SimTime> = obs.iter().map(|(_, t)| *t).collect();
    for w in times.windows(2) {
        assert!(
            w[0] <= w[1],
            "observed event times must be non-decreasing, got pair ({}, {}) in {:?}",
            w[0],
            w[1],
            times,
        );
    }
}
