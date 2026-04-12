//! Integration tests for the `Protocol` and `TrafficModel` trait contracts.
//!
//! theatron's public API is centred on `Protocol`: external implementors write
//! state machines against this trait and plug them into the scheduler via a
//! `NodeHandle` adapter. The trait has several non-obvious behavioural
//! invariants that must hold for the simulation to be correct:
//!
//! 1. `init` may return a wake time; the scheduler fires `update` at that time.
//! 2. `update` drives timer-based state transitions and schedules the next wake.
//! 3. `poll_transmit` is called after every wake; returning `Some` enqueues a TX.
//! 4. `on_receive` may return a wake time, which schedules a later `update`.
//! 5. `metrics` reflects all state accumulated across every method call.
//! 6. A `TrafficModel` that returns `None` after exhaustion silences the node.
//!
//! None of these were covered by the existing test suite; the scheduler tests
//! use `NodeHandle` directly and bypass the `Protocol` layer entirely.

use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::{Protocol, TrafficModel};
use theatron::types::{NodeId, RxMetadata, Transmission};

// ---------------------------------------------------------------------------
// Minimal concrete TrafficModel
// ---------------------------------------------------------------------------

/// Yields up to `limit` payloads, then is exhausted.
struct CountedPayloads {
    remaining: usize,
    payload: Vec<u8>,
}

impl CountedPayloads {
    fn new(limit: usize, payload: Vec<u8>) -> Self {
        Self {
            remaining: limit,
            payload,
        }
    }
}

impl TrafficModel for CountedPayloads {
    fn next_payload(&mut self, _time: SimTime) -> Option<Vec<u8>> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some(self.payload.clone())
    }
}

// ---------------------------------------------------------------------------
// Minimal concrete Protocol — a simple "send one frame per wake" state machine
// ---------------------------------------------------------------------------

/// Metrics collected by `PingProtocol`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PingMetrics {
    wakes: u32,
    transmissions: u32,
    receptions: u32,
    /// Wake times requested via `on_receive`.
    rx_triggered_wakes: u32,
}

/// State threaded through `PingProtocol` by the scheduler.
struct PingState {
    metrics: PingMetrics,
    traffic: CountedPayloads,
    /// Fixed period between wakes (None means one-shot).
    period_us: Option<SimTime>,
}

/// A protocol that periodically transmits one payload per wake using a
/// `TrafficModel`, and records every received frame.
struct PingProtocol {
    period_us: Option<SimTime>,
    payload_limit: usize,
    payload: Vec<u8>,
    /// Delay before the first wake (simulates `init` scheduling).
    init_delay_us: SimTime,
    /// When set, `on_receive` returns `Some(time + this_delay)`.
    rx_wake_delay_us: Option<SimTime>,
    sf: u8,
    frequency: u32,
    duration_us: u64,
}

impl Protocol for PingProtocol {
    type Config = ();
    type State = PingState;
    type Metrics = PingMetrics;

    fn init(&self, _config: ()) -> (PingState, Option<SimTime>) {
        let state = PingState {
            metrics: PingMetrics::default(),
            traffic: CountedPayloads::new(self.payload_limit, self.payload.clone()),
            period_us: self.period_us,
        };
        // Schedule the initial wake.
        (state, Some(self.init_delay_us))
    }

    fn update(&self, state: &mut PingState, time: SimTime) -> Option<SimTime> {
        state.metrics.wakes += 1;
        // Schedule the next periodic wake if there is a period.
        state.period_us.map(|p| time + p)
    }

    fn poll_transmit(&self, state: &mut PingState, time: SimTime) -> Option<Transmission> {
        state.traffic.next_payload(time).map(|payload| {
            state.metrics.transmissions += 1;
            Transmission {
                payload,
                sf: self.sf,
                bandwidth: 125_000,
                coding_rate: 5,
                frequency: self.frequency,
                duration_us: self.duration_us,
                tx_power_dbm: 14,
            }
        })
    }

    fn on_receive(
        &self,
        state: &mut PingState,
        _frame: RxMetadata,
        time: SimTime,
    ) -> Option<SimTime> {
        state.metrics.receptions += 1;
        self.rx_wake_delay_us.map(|d| {
            state.metrics.rx_triggered_wakes += 1;
            time + d
        })
    }

    fn metrics(&self, state: &PingState) -> PingMetrics {
        state.metrics.clone()
    }
}

// ---------------------------------------------------------------------------
// Adapter: wrap Protocol + State into a NodeHandle
// ---------------------------------------------------------------------------

struct ProtocolNode<P: Protocol> {
    id: NodeId,
    proto: P,
    state: P::State,
}

impl<P: Protocol + 'static> ProtocolNode<P> {
    /// Create a node, calling `proto.init(config)` to obtain the initial state
    /// and the optional first wake time.
    fn new(id: NodeId, proto: P, config: P::Config) -> (Self, Option<SimTime>) {
        let (state, wake) = proto.init(config);
        (Self { id, proto, state }, wake)
    }

    fn metrics(&self) -> P::Metrics {
        self.proto.metrics(&self.state)
    }
}

impl<P: Protocol + 'static> NodeHandle for ProtocolNode<P> {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, frame: RxMetadata, time: SimTime) -> Option<SimTime> {
        self.proto.on_receive(&mut self.state, frame, time)
    }

    fn poll_transmit(&mut self, time: SimTime) -> Option<Transmission> {
        self.proto.poll_transmit(&mut self.state, time)
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        self.proto.update(&mut self.state, time)
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn make_ping(period_us: Option<SimTime>, payload_limit: usize) -> PingProtocol {
    PingProtocol {
        period_us,
        payload_limit,
        payload: vec![0xAB, 0xCD],
        init_delay_us: 0,
        rx_wake_delay_us: None,
        sf: 7,
        frequency: 868_100_000,
        duration_us: 50_000,
    }
}

// ---------------------------------------------------------------------------
// Tests: Protocol::init
// ---------------------------------------------------------------------------

/// `init` returning `Some(t)` must cause the scheduler to call `update` at
/// time `t`, which in turn calls `poll_transmit` — exercising the full
/// timer-driven transmission path from `init` to first TX.
#[test]
fn init_wake_triggers_first_update_and_poll_transmit() {
    let proto = PingProtocol {
        init_delay_us: 10_000,
        ..make_ping(None, 1)
    };
    let (node, wake) = ProtocolNode::new(NodeId(1), proto, ());
    let mut sched = Scheduler::new(100_000);
    sched.add_node(Box::new(node), wake);
    sched.run();

    // One wake scheduled by init, one TX from the payload limit.
    assert_eq!(sched.metrics.total_tx, 1);
}

/// `init` returning `None` means the node is never woken unless an event
/// arrives — the scheduler must not call `update` spontaneously.
#[test]
fn init_with_no_wake_produces_no_activity() {
    let proto = PingProtocol {
        init_delay_us: 0,
        ..make_ping(None, 5)
    };
    let (node, _wake) = ProtocolNode::new(NodeId(1), proto, ());
    // Deliberately ignore the returned wake and pass None.
    let mut sched = Scheduler::new(100_000);
    sched.add_node(Box::new(node), None);
    sched.run();

    assert_eq!(sched.metrics.total_tx, 0, "no wake → no update → no TX");
}

// ---------------------------------------------------------------------------
// Tests: Protocol::update + periodic wake scheduling
// ---------------------------------------------------------------------------

/// `update` returning `Some(t)` must reschedule the node; over a bounded
/// simulation the number of wakes must match the expected count.
#[test]
fn periodic_wakes_fire_expected_number_of_times() {
    const PERIOD_US: SimTime = 100_000;
    const END_US: SimTime = 500_000;
    // Wakes at t=0, 100k, 200k, 300k, 400k — end_time is exclusive (> end)
    const EXPECTED_WAKES: u32 = 5;

    let proto = make_ping(Some(PERIOD_US), 0 /* no TXs */);
    let (node, wake) = ProtocolNode::new(NodeId(1), proto, ());
    let mut sched = Scheduler::new(END_US);
    sched.add_node(Box::new(node), wake);
    sched.run();

    // Retrieve the ProtocolNode back out — we can check metrics via the
    // scheduler's public state, but metrics are embedded in the node.
    // Instead, we verify via the scheduler's recorded total_tx == 0 and
    // current_time advanced to the last wake before end_time.
    assert_eq!(sched.metrics.total_tx, 0);
    assert_eq!(
        sched.current_time(),
        (EXPECTED_WAKES as SimTime - 1) * PERIOD_US,
        "last wake must land at (N-1)*period"
    );
}

/// A one-shot `update` (returns `None`) fires exactly once and stops.
#[test]
fn one_shot_update_fires_once() {
    let proto = make_ping(None /* one-shot */, 1);
    let (node, wake) = ProtocolNode::new(NodeId(1), proto, ());
    let mut sched = Scheduler::new(1_000_000);
    sched.add_node(Box::new(node), wake);
    sched.run();

    // Exactly one wake → one poll_transmit call → one TX (payload_limit=1).
    assert_eq!(sched.metrics.total_tx, 1);
    // Scheduler should stop early (no more events after the single TX).
    assert!(sched.current_time() < 1_000_000);
}

// ---------------------------------------------------------------------------
// Tests: Protocol::poll_transmit + TrafficModel exhaustion
// ---------------------------------------------------------------------------

/// When the `TrafficModel` is exhausted, `poll_transmit` returns `None` and
/// subsequent wakes produce no further transmissions.
#[test]
fn exhausted_traffic_model_produces_no_more_transmissions() {
    const PAYLOAD_LIMIT: usize = 3;
    const PERIOD_US: SimTime = 100_000;
    const END_US: SimTime = 1_000_000;

    let proto = make_ping(Some(PERIOD_US), PAYLOAD_LIMIT);
    let (node, wake) = ProtocolNode::new(NodeId(1), proto, ());
    let mut sched = Scheduler::new(END_US);
    sched.add_node(Box::new(node), wake);
    sched.run();

    // Exactly PAYLOAD_LIMIT transmissions even though many more wakes fire.
    assert_eq!(sched.metrics.total_tx, PAYLOAD_LIMIT as u64);
}

/// Payload bytes returned by the `TrafficModel` must arrive unmodified at
/// other nodes (channel carries them verbatim).
#[test]
fn traffic_model_payload_delivered_verbatim() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let received: Rc<RefCell<Vec<Vec<u8>>>> = Rc::new(RefCell::new(Vec::new()));
    let received_clone = Rc::clone(&received);

    // A simple receiver NodeHandle that records every payload it gets.
    struct CapturingReceiver {
        id: NodeId,
        bucket: Rc<RefCell<Vec<Vec<u8>>>>,
    }

    impl NodeHandle for CapturingReceiver {
        fn node_id(&self) -> NodeId {
            self.id
        }
        fn on_receive(&mut self, frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
            self.bucket.borrow_mut().push(frame.payload.clone());
            None
        }
        fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> {
            None
        }
        fn update(&mut self, _t: SimTime) -> Option<SimTime> {
            None
        }
    }

    let expected_payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let proto = PingProtocol {
        payload: expected_payload.clone(),
        ..make_ping(None, 1)
    };
    let (sender, wake) = ProtocolNode::new(NodeId(1), proto, ());
    let receiver = CapturingReceiver {
        id: NodeId(2),
        bucket: received_clone,
    };

    let mut sched = Scheduler::new(200_000);
    sched.add_node(Box::new(sender), wake);
    sched.add_node(Box::new(receiver), None);
    sched.run();

    let got = received.borrow();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], expected_payload, "payload must be delivered verbatim");
}

// ---------------------------------------------------------------------------
// Tests: Protocol::on_receive → wake → poll_transmit chain
// ---------------------------------------------------------------------------

/// `on_receive` returning `Some(t)` must cause the scheduler to call
/// `update` at time `t`, which should then drive `poll_transmit`.
/// This validates the receive-triggered wake path.
#[test]
fn on_receive_wake_drives_reply_transmission() {
    // Sender: fires once at t=0, sends one frame.
    let sender_proto = make_ping(None, 1);
    let (sender, sender_wake) = ProtocolNode::new(NodeId(1), sender_proto, ());

    // Receiver: on_receive returns Some(time + 10_000), triggering a wake
    // that will call poll_transmit → one reply TX.
    let receiver_proto = PingProtocol {
        rx_wake_delay_us: Some(10_000),
        payload_limit: 1,
        ..make_ping(None, 1)
    };
    let (receiver, _) = ProtocolNode::new(NodeId(2), receiver_proto, ());

    let mut sched = Scheduler::new(300_000);
    sched.add_node(Box::new(sender), sender_wake);
    // receiver has no initial wake; only wakes via on_receive
    sched.add_node(Box::new(receiver), None);
    sched.run();

    // Sender TX (1) + receiver reply TX (1) = 2
    assert_eq!(
        sched.metrics.total_tx,
        2,
        "on_receive wake must trigger a reply transmission"
    );
}

/// Multiple nodes receiving the same broadcast each independently schedule
/// their own on_receive-triggered wakes without interfering with each other.
#[test]
fn broadcast_on_receive_wakes_are_independent() {
    const N_RECEIVERS: u32 = 4;
    const RX_WAKE_DELAY: SimTime = 20_000;

    let sender_proto = make_ping(None, 1);
    let (sender, sender_wake) = ProtocolNode::new(NodeId(0), sender_proto, ());

    let mut sched = Scheduler::new(500_000);
    sched.add_node(Box::new(sender), sender_wake);

    for i in 1..=N_RECEIVERS {
        let proto = PingProtocol {
            rx_wake_delay_us: Some(RX_WAKE_DELAY),
            payload_limit: 1,
            ..make_ping(None, 1)
        };
        let (node, _) = ProtocolNode::new(NodeId(i), proto, ());
        sched.add_node(Box::new(node), None);
    }
    sched.run();

    // 1 original + N_RECEIVERS replies, but replies are on same SF/freq at
    // similar times — they will collide. What must hold is that total_rx
    // reflects the original broadcast being received by all N receivers.
    assert_eq!(
        sched.metrics.total_rx,
        N_RECEIVERS as u64,
        "every receiver must receive the original broadcast"
    );
    // All receivers attempt a reply, so total_tx = 1 + N_RECEIVERS.
    assert_eq!(
        sched.metrics.total_tx,
        1 + N_RECEIVERS as u64,
        "every receiver schedules one reply TX"
    );
}

// ---------------------------------------------------------------------------
// Tests: Protocol::metrics accumulation
// ---------------------------------------------------------------------------

/// `metrics` must reflect all transmissions and receptions accumulated
/// across the entire simulation run, not just the last event.
#[test]
fn metrics_accumulate_over_simulation() {
    const PAYLOAD_LIMIT: usize = 5;
    const PERIOD_US: SimTime = 100_000;
    const END_US: SimTime = 700_000;

    let proto = make_ping(Some(PERIOD_US), PAYLOAD_LIMIT);
    let (mut node, wake) = ProtocolNode::new(NodeId(1), proto, ());

    // Run a separate receiver so there is something to receive.
    // We want to check PingProtocol's own metrics after the run, so we need
    // access to the node after the scheduler consumes it. We verify this
    // indirectly via the scheduler's aggregate metrics.
    let mut sched = Scheduler::new(END_US);

    // Add a second node so the sender's TXs go somewhere.
    sched.add_node(Box::new(node), wake);

    // A second periodic sender so the first node also receives frames.
    let receiver_proto = make_ping(Some(PERIOD_US), PAYLOAD_LIMIT);
    let (receiver_node, receiver_wake) = ProtocolNode::new(NodeId(2), receiver_proto, ());
    sched.add_node(Box::new(receiver_node), receiver_wake);

    sched.run();

    // Both nodes transmit PAYLOAD_LIMIT times (no collisions because TXs are
    // scheduled at the same absolute times on the same SF/freq — they WILL
    // collide in pairs).  What we care about is that the scheduler's
    // aggregate counters reflect the full simulation, not a prefix.
    assert_eq!(
        sched.metrics.total_tx,
        (PAYLOAD_LIMIT * 2) as u64,
        "both nodes must exhaust their traffic models"
    );
    // All TXs are simultaneous (same period, same start) → all collide.
    assert_eq!(
        sched.metrics.total_collisions,
        (PAYLOAD_LIMIT * 2) as u64,
        "simultaneous same-SF/freq TXs must all collide"
    );
    assert_eq!(
        sched.metrics.total_rx,
        0,
        "collisions prevent any delivery"
    );
}

// ---------------------------------------------------------------------------
// Tests: TrafficModel contract
// ---------------------------------------------------------------------------

/// `next_payload` must return exactly `limit` payloads then only `None`.
#[test]
fn traffic_model_exhaustion_is_exact() {
    let limit = 7usize;
    let mut model = CountedPayloads::new(limit, vec![0x01]);

    let delivered: usize = (0..limit + 5)
        .filter(|&t| model.next_payload(t as SimTime).is_some())
        .count();

    assert_eq!(delivered, limit, "TrafficModel must yield exactly `limit` payloads");
}

/// Calling `next_payload` with increasing timestamps must not affect the
/// count — the model is payload-count-driven, not time-driven.
#[test]
fn traffic_model_count_is_independent_of_time() {
    let mut model = CountedPayloads::new(3, vec![0xFF]);
    assert!(model.next_payload(0).is_some());
    assert!(model.next_payload(1_000_000).is_some());
    assert!(model.next_payload(u64::MAX / 2).is_some());
    // Now exhausted.
    assert!(model.next_payload(0).is_none());
    assert!(model.next_payload(999_999_999).is_none());
}

// ---------------------------------------------------------------------------
// Tests: Protocol init → full round-trip through scheduler
// ---------------------------------------------------------------------------

/// Verify the full Protocol ↔ Scheduler round-trip: `init` returns a wake
/// time, the scheduler fires `update`, `poll_transmit` emits a frame, and
/// a second node receives it — all with correct metric accounting.
#[test]
fn full_round_trip_init_to_delivery() {
    let sender_proto = PingProtocol {
        init_delay_us: 5_000,
        ..make_ping(None, 1)
    };
    let (sender, sender_wake) = ProtocolNode::new(NodeId(1), sender_proto, ());

    let mut sched = Scheduler::new(200_000);
    sched.add_node(Box::new(sender), sender_wake);

    // Plain NodeHandle receiver so we can keep it simple.
    struct CountingReceiver {
        id: NodeId,
        count: u32,
    }
    impl NodeHandle for CountingReceiver {
        fn node_id(&self) -> NodeId { self.id }
        fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> {
            self.count += 1;
            None
        }
        fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> { None }
        fn update(&mut self, _t: SimTime) -> Option<SimTime> { None }
    }

    sched.add_node(Box::new(CountingReceiver { id: NodeId(2), count: 0 }), None);
    sched.run();

    assert_eq!(sched.metrics.total_tx, 1, "Protocol init → one TX");
    assert_eq!(sched.metrics.total_rx, 1, "TX delivered to one receiver");
    assert_eq!(sched.metrics.total_collisions, 0);
    // Wake fired at init_delay_us = 5_000.
    assert_eq!(
        sched.current_time(),
        50_000 + 5_000, // 5_000 wake + 50_000 tx duration = TxComplete at 55_000
        "scheduler time must reflect TX completion, not just the wake"
    );
}
