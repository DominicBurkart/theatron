//! Tests for the `Protocol` trait contract and the scheduler's timer-delivery invariant.
//!
//! # What is the timer contract?
//!
//! When `Protocol::update` (or `NodeHandle::update`) returns `Some(t)`, the scheduler
//! is required to call `update` again at *exactly* time `t` — no earlier, no later.
//! These tests verify that invariant holds end-to-end through `Scheduler::run`.
//!
//! # Why does this matter?
//!
//! The `Protocol` trait is the central abstraction in theatron, yet it had zero test
//! coverage before this file.  Any regression in the scheduler's wake-scheduling path
//! could silently break all protocol implementations.

use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::Protocol;
use theatron::types::{NodeId, RxMetadata, Transmission};

// ---------------------------------------------------------------------------
// TwoPhaseProtocol — a minimal Protocol implementation used by the tests
// ---------------------------------------------------------------------------

/// A two-phase protocol that:
///
/// 1. At t = 0 transmits one beacon frame and asks to be woken at t = 1_000_000.
/// 2. At t = 1_000_000 records the exact wake time so the test can assert it.
///
/// This is intentionally the smallest possible `Protocol` that exercises both
/// the transmit path and the deferred-wake (timer) path.
struct TwoPhaseProtocol;

struct TwoPhaseState {
    /// Set to `true` once `update` has been called at the RX-window time.
    rx_window_fired: bool,
    /// The `SimTime` value passed to the second `update` call (the RX window).
    rx_window_time: Option<SimTime>,
    /// Number of frames queued for transmission by `poll_transmit`.
    transmit_count: u32,
}

impl Protocol for TwoPhaseProtocol {
    type Config = ();
    type State = TwoPhaseState;
    type Metrics = u32; // returns the transmit count

    fn init(&self, _config: ()) -> (TwoPhaseState, Option<SimTime>) {
        let state = TwoPhaseState {
            rx_window_fired: false,
            rx_window_time: None,
            transmit_count: 0,
        };
        // Wake immediately at t = 0 so `update` → `poll_transmit` fires.
        (state, Some(0))
    }

    fn on_receive(
        &self,
        _state: &mut TwoPhaseState,
        _frame: RxMetadata,
        _time: SimTime,
    ) -> Option<SimTime> {
        None
    }

    fn poll_transmit(&self, state: &mut TwoPhaseState, _time: SimTime) -> Option<Transmission> {
        // Emit exactly one beacon frame the first time we are polled.
        if state.transmit_count == 0 {
            state.transmit_count += 1;
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

    fn update(&self, state: &mut TwoPhaseState, time: SimTime) -> Option<SimTime> {
        if !state.rx_window_fired {
            // Phase 1 — initial wake.  Schedule the RX window.
            state.rx_window_fired = true;
            Some(1_000_000)
        } else {
            // Phase 2 — RX-window wake.  Record the exact time for assertion.
            state.rx_window_time = Some(time);
            None // no further wakes
        }
    }

    fn metrics(&self, state: &TwoPhaseState) -> u32 {
        state.transmit_count
    }
}

// ---------------------------------------------------------------------------
// ProtocolNode — bridges Protocol + State into NodeHandle
// ---------------------------------------------------------------------------

/// Wraps a `Protocol` implementation together with its mutable `State` so that
/// the pair can be registered with `Scheduler` as a `NodeHandle`.
///
/// This is the idiomatic glue layer between the stateless `Protocol` trait and
/// the stateful `NodeHandle` that the scheduler drives.
struct ProtocolNode<P: Protocol> {
    id: NodeId,
    protocol: P,
    state: P::State,
}

impl<P: Protocol> ProtocolNode<P> {
    /// Construct a new node, calling `Protocol::init` to obtain the initial state.
    /// Returns `(Self, initial_wake)` so the caller can pass `initial_wake` to
    /// `Scheduler::add_node`.
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
/// `update`.  This test verifies that invariant for a `Protocol` implementation
/// that asks to be woken at t = 1_000_000.
#[test]
fn scheduler_delivers_wake_at_exact_scheduled_time() {
    const RX_WINDOW: SimTime = 1_000_000;

    let (_node, _initial_wake) = ProtocolNode::new(NodeId(1), TwoPhaseProtocol, ());
    // Downcast to Box<dyn NodeHandle> — we need to keep a raw pointer so we can
    // inspect state after the run.  Instead, we run the scheduler and then query
    // the metrics (which encode the transmit count).  The wake-time assertion is
    // encoded in the protocol state; we recover it through a second, post-run
    // `ProtocolNode` that we build just for introspection.
    //
    // Simpler approach: use a shared-state wrapper via a raw pointer that is
    // valid for the duration of the test.  But the cleanest approach for this
    // codebase is to keep everything owned.  We therefore use a two-node setup
    // where one node records the observation and we extract it from `metrics`.
    //
    // Actually the cleanest approach: keep the `ProtocolNode` in a `Box`, run
    // the scheduler, and downcast back.  `Box<dyn NodeHandle>` doesn't support
    // downcasting, so we'll use `unsafe` pointer aliasing to peek at state.
    //
    // The simplest correct approach: use `std::rc::Rc<RefCell<…>>` for shared
    // state.  We go with that to keep things readable.

    // Re-implement with shared state so we can observe after the run.
    use std::cell::RefCell;
    use std::rc::Rc;

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

    // Shared observable state
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

    // --- Timer contract: the scheduler must have advanced to at least RX_WINDOW ---
    assert!(
        sched.current_time() >= RX_WINDOW,
        "scheduler did not advance to the scheduled wake time: got {}, expected >= {}",
        sched.current_time(),
        RX_WINDOW,
    );

    // --- Timer contract: update must have been called at exactly RX_WINDOW ---
    let observed_time = shared.borrow().rx_window_time;
    assert_eq!(
        observed_time,
        Some(RX_WINDOW),
        "update was not called at the exact scheduled time: got {:?}, expected Some({})",
        observed_time,
        RX_WINDOW,
    );

    // --- Transmission count: exactly one frame was sent ---
    assert_eq!(
        sched.metrics.total_tx, 1,
        "expected exactly 1 transmission, got {}",
        sched.metrics.total_tx,
    );
}

/// Smoke-test the `ProtocolNode` bridge: a `Protocol` that never transmits
/// and never requests a wake-up should run to completion without panicking,
/// with zero transmissions recorded.
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

/// Verify that a `Protocol` whose `init` returns an initial wake time of `Some(0)`
/// causes `update` to be called at t = 0 before the simulation ends.
#[test]
fn protocol_init_wake_at_zero_fires_update() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let update_called = Rc::new(RefCell::new(false));

    struct WakeAtZeroProtocol {
        flag: Rc<RefCell<bool>>,
    }

    impl Protocol for WakeAtZeroProtocol {
        type Config = ();
        type State = ();
        type Metrics = ();

        fn init(&self, _: ()) -> ((), Option<SimTime>) {
            ((), Some(0)) // request immediate wake
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
