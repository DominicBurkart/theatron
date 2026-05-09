//! Tests for the `Protocol` trait contract and the scheduler's timer-delivery invariant.
//!
//! # Timer contract
//!
//! When `Protocol::update` (or `NodeHandle::update`) returns `Some(t)`, the scheduler
//! must call `update` again at *exactly* time `t` — no earlier, no later. These tests
//! verify that invariant holds end-to-end through [`Scheduler::run`].
//!
//! The `Protocol` trait is the central abstraction (see [`ARCHITECTURE.md`](../ARCHITECTURE.md));
//! any regression in the scheduler's wake-scheduling path could silently break every
//! protocol implementation.

use std::cell::RefCell;
use std::rc::Rc;

use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::Protocol;
use theatron::types::{NodeId, RxMetadata, Transmission};

// ---------------------------------------------------------------------------
// ProtocolNode — bridges Protocol + State into NodeHandle
// ---------------------------------------------------------------------------

/// Wraps a `Protocol` implementation together with its mutable `State` so the
/// pair can be registered with `Scheduler` as a `NodeHandle`. This is the
/// idiomatic glue layer between the stateless `Protocol` trait and the
/// stateful `NodeHandle` the scheduler drives.
struct ProtocolNode<P: Protocol> {
    id: NodeId,
    protocol: P,
    state: P::State,
}

impl<P: Protocol> ProtocolNode<P> {
    /// Construct a new node, calling `Protocol::init` to obtain the initial state.
    /// Returns `(Self, initial_wake)` so the caller can pass `initial_wake` to
    /// [`Scheduler::add_node`].
    fn new(id: NodeId, protocol: P, config: P::Config) -> (Self, Option<SimTime>) {
        let (state, wake) = protocol.init(config);
        (
            Self {
                id,
                protocol,
                state,
            },
            wake,
        )
    }
}

impl<P: Protocol + 'static> NodeHandle for ProtocolNode<P>
where
    P::State: 'static,
{
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, frame: RxMetadata, time: SimTime) -> Option<SimTime> {
        self.protocol.on_receive(&mut self.state, frame, time)
    }

    fn poll_transmit(&mut self, time: SimTime) -> Option<Transmission> {
        self.protocol.poll_transmit(&mut self.state, time)
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        self.protocol.update(&mut self.state, time)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The scheduler must call `update` at the *exact* time returned by a previous
/// `update`. Verified here with a two-phase `Protocol` whose state lives in an
/// `Rc<RefCell<_>>` so the test can inspect it after `Scheduler::run` returns.
#[test]
fn scheduler_delivers_wake_at_exact_scheduled_time() {
    const RX_WINDOW: SimTime = 1_000_000;

    struct ObservingState {
        rx_window_fired: bool,
        rx_window_time: Option<SimTime>,
        transmit_count: u32,
    }

    struct ObservingProtocol {
        shared: Rc<RefCell<ObservingState>>,
    }

    impl Protocol for ObservingProtocol {
        type Config = ();
        type State = (); // state lives in the Rc
        type Metrics = ();

        fn init(&self, _: ()) -> ((), Option<SimTime>) {
            ((), Some(0))
        }

        fn on_receive(&self, _: &mut (), _: RxMetadata, _: SimTime) -> Option<SimTime> {
            None
        }

        fn poll_transmit(&self, _: &mut (), _: SimTime) -> Option<Transmission> {
            let mut s = self.shared.borrow_mut();
            if s.transmit_count == 0 {
                s.transmit_count += 1;
                Some(Transmission {
                    payload: vec![0xBE, 0xAC, 0x04],
                    sf: 7,
                    bandwidth: 125_000,
                    coding_rate: 5,
                    frequency: 868_100_000,
                    duration_us: 50_000,
                    tx_power_dbm: 14,
                })
            } else {
                None
            }
        }

        fn update(&self, _: &mut (), time: SimTime) -> Option<SimTime> {
            let mut s = self.shared.borrow_mut();
            if !s.rx_window_fired {
                s.rx_window_fired = true;
                Some(RX_WINDOW)
            } else {
                s.rx_window_time = Some(time);
                None
            }
        }

        fn metrics(&self, _: &()) {}
    }

    let shared = Rc::new(RefCell::new(ObservingState {
        rx_window_fired: false,
        rx_window_time: None,
        transmit_count: 0,
    }));

    let protocol = ObservingProtocol {
        shared: Rc::clone(&shared),
    };
    let (node, initial_wake) = ProtocolNode::new(NodeId(1), protocol, ());

    let mut sched = Scheduler::new(2_000_000);
    sched.add_node(Box::new(node), initial_wake);
    sched.run();

    // Timer contract: the scheduler must have advanced to at least RX_WINDOW.
    assert!(
        sched.current_time() >= RX_WINDOW,
        "scheduler did not advance to the scheduled wake time: got {}, expected >= {}",
        sched.current_time(),
        RX_WINDOW,
    );

    // Timer contract: update must have been called at exactly RX_WINDOW.
    let observed_time = shared.borrow().rx_window_time;
    assert_eq!(
        observed_time,
        Some(RX_WINDOW),
        "update was not called at the exact scheduled time: got {:?}, expected Some({})",
        observed_time,
        RX_WINDOW,
    );

    assert_eq!(
        sched.metrics.total_tx, 1,
        "expected exactly 1 transmission, got {}",
        sched.metrics.total_tx,
    );
}

/// Smoke-test the `ProtocolNode` bridge: a `Protocol` that never transmits and
/// never requests a wake-up runs to completion without panicking, with zero
/// transmissions recorded.
#[test]
fn protocol_node_no_op_does_not_panic() {
    struct NoOpProtocol;

    impl Protocol for NoOpProtocol {
        type Config = ();
        type State = ();
        type Metrics = ();

        fn init(&self, _: ()) -> ((), Option<SimTime>) {
            ((), None)
        }
        fn on_receive(&self, _: &mut (), _: RxMetadata, _: SimTime) -> Option<SimTime> {
            None
        }
        fn poll_transmit(&self, _: &mut (), _: SimTime) -> Option<Transmission> {
            None
        }
        fn update(&self, _: &mut (), _: SimTime) -> Option<SimTime> {
            None
        }
        fn metrics(&self, _: &()) {}
    }

    let (node, initial_wake) = ProtocolNode::new(NodeId(2), NoOpProtocol, ());
    let mut sched = Scheduler::new(1_000_000);
    sched.add_node(Box::new(node), initial_wake);
    sched.run();

    assert_eq!(sched.metrics.total_tx, 0);
}

/// A `Protocol` whose `init` returns `Some(0)` must have `update` called at t = 0.
#[test]
fn protocol_init_wake_at_zero_fires_update() {
    let update_called = Rc::new(RefCell::new(false));

    struct WakeAtZeroProtocol {
        flag: Rc<RefCell<bool>>,
    }

    impl Protocol for WakeAtZeroProtocol {
        type Config = ();
        type State = ();
        type Metrics = ();

        fn init(&self, _: ()) -> ((), Option<SimTime>) {
            ((), Some(0))
        }
        fn on_receive(&self, _: &mut (), _: RxMetadata, _: SimTime) -> Option<SimTime> {
            None
        }
        fn poll_transmit(&self, _: &mut (), _: SimTime) -> Option<Transmission> {
            None
        }
        fn update(&self, _: &mut (), _time: SimTime) -> Option<SimTime> {
            *self.flag.borrow_mut() = true;
            None
        }
        fn metrics(&self, _: &()) {}
    }

    let (node, initial_wake) = ProtocolNode::new(
        NodeId(3),
        WakeAtZeroProtocol {
            flag: Rc::clone(&update_called),
        },
        (),
    );

    let mut sched = Scheduler::new(1_000_000);
    sched.add_node(Box::new(node), initial_wake);
    sched.run();

    assert!(
        *update_called.borrow(),
        "update was never called even though init returned Some(0)"
    );
}

/// Events scheduled at the same `SimTime` must fire in the order they were
/// scheduled (FIFO via the scheduler's internal seq counter). Without this
/// invariant, simulations are non-deterministic when multiple nodes wake at
/// the same instant.
///
/// Setup: three nodes each request a wake at t = 50_000 (registered in order
/// 1, 2, 3). Each node records the *observation order* in which `update` was
/// actually invoked via a shared counter. The recorded order must match the
/// registration order.
#[test]
fn same_time_events_fire_in_fifo_order() {
    let counter = Rc::new(RefCell::new(0u32));
    let observations: Rc<RefCell<Vec<(NodeId, u32)>>> = Rc::new(RefCell::new(Vec::new()));

    struct RecordingNode {
        id: NodeId,
        counter: Rc<RefCell<u32>>,
        observations: Rc<RefCell<Vec<(NodeId, u32)>>>,
        wake_at: Option<SimTime>,
    }

    impl NodeHandle for RecordingNode {
        fn node_id(&self) -> NodeId {
            self.id
        }
        fn on_receive(&mut self, _: RxMetadata, _: SimTime) -> Option<SimTime> {
            None
        }
        fn poll_transmit(&mut self, _: SimTime) -> Option<Transmission> {
            None
        }
        fn update(&mut self, _: SimTime) -> Option<SimTime> {
            let mut c = self.counter.borrow_mut();
            *c += 1;
            self.observations.borrow_mut().push((self.id, *c));
            self.wake_at.take()
        }
    }

    let mut sched = Scheduler::new(100_000);
    for id in [1, 2, 3] {
        sched.add_node(
            Box::new(RecordingNode {
                id: NodeId(id),
                counter: Rc::clone(&counter),
                observations: Rc::clone(&observations),
                wake_at: None,
            }),
            Some(50_000),
        );
    }
    sched.run();

    let obs = observations.borrow();
    assert_eq!(obs.len(), 3, "expected exactly 3 updates at the same time");
    assert_eq!(
        obs.iter().map(|(id, _)| id.0).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "updates must fire in registration order when times are equal"
    );
}
