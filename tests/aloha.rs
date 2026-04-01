//! Integration tests for the Pure ALOHA reference protocol.
//!
//! Three tests:
//!   1. `single_node_delivery`    — one sender, one packet → received.
//!   2. `multi_node_collision`    — two simultaneous senders → collision.
//!   3. `backoff_retransmission`  — backoff actually delays the next TX.

use std::cell::RefCell;
use std::rc::Rc;

use theatron::aloha::{AlohaNode, PoissonTraffic, LORA_SF7_DURATION_US};
use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::TrafficModel;
use theatron::types::{NodeId, RxMetadata, Transmission};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A traffic model that fires exactly once when `time >= fire_at`.
struct FireOnce {
    payload: Vec<u8>,
    fire_at: SimTime,
    fired: bool,
}

impl FireOnce {
    fn new(payload: Vec<u8>, fire_at: SimTime) -> Self {
        Self {
            payload,
            fire_at,
            fired: false,
        }
    }
}

impl TrafficModel for FireOnce {
    fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>> {
        if !self.fired && time >= self.fire_at {
            self.fired = true;
            Some(self.payload.clone())
        } else {
            None
        }
    }
}

/// A pure receiver node that counts received frames.
struct Receiver {
    id: NodeId,
}

impl Receiver {
    fn new(id: u32) -> Self {
        Self { id: NodeId(id) }
    }
}

impl NodeHandle for Receiver {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        None
    }
    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

/// Traffic model that fires exactly `n` times, one payload per poll.
struct NShot(u8);

impl TrafficModel for NShot {
    fn next_payload(&mut self, _time: SimTime) -> Option<Vec<u8>> {
        if self.0 > 0 {
            self.0 -= 1;
            Some(vec![self.0])
        } else {
            None
        }
    }
}

/// Wraps an `AlohaNode` and records the simulation time of each TX via a
/// shared `Rc<RefCell<Vec<SimTime>>>` so callers can inspect timings after
/// `scheduler.run()` consumes the boxed node.
struct RecordingNode<T: TrafficModel> {
    inner: AlohaNode<T>,
    tx_times: Rc<RefCell<Vec<SimTime>>>,
}

impl<T: TrafficModel> NodeHandle for RecordingNode<T> {
    fn node_id(&self) -> NodeId {
        self.inner.node_id()
    }
    fn on_receive(&mut self, frame: RxMetadata, time: SimTime) -> Option<SimTime> {
        self.inner.on_receive(frame, time)
    }
    fn poll_transmit(&mut self, time: SimTime) -> Option<Transmission> {
        let tx = self.inner.poll_transmit(time);
        if tx.is_some() {
            self.tx_times.borrow_mut().push(time);
        }
        tx
    }
    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        self.inner.update(time)
    }
}

// ---------------------------------------------------------------------------
// Test 1: single_node_delivery
// ---------------------------------------------------------------------------

/// One sender fires exactly one packet; the single receiver must receive it.
#[test]
fn single_node_delivery() {
    let sim_end = LORA_SF7_DURATION_US * 10;
    let mut scheduler = Scheduler::new(sim_end);

    let traffic = FireOnce::new(vec![0xAB], 0);
    let sender = AlohaNode::new(NodeId(1), traffic, 0, 1);
    scheduler.add_node(Box::new(sender), Some(0));
    scheduler.add_node(Box::new(Receiver::new(2)), None);

    scheduler.run();

    assert_eq!(scheduler.metrics.total_tx, 1, "exactly one TX expected");
    assert_eq!(
        scheduler.metrics.total_rx, 1,
        "single packet must be delivered to the receiver"
    );
    assert_eq!(
        scheduler.metrics.total_collisions, 0,
        "no collision with a single sender"
    );
}

// ---------------------------------------------------------------------------
// Test 2: multi_node_collision
// ---------------------------------------------------------------------------

/// Two senders transmit at the same time on the same SF/frequency; the
/// channel model must detect the collision (both frames collided, zero
/// deliveries).
#[test]
fn multi_node_collision() {
    let sim_end = LORA_SF7_DURATION_US * 10;
    let mut scheduler = Scheduler::new(sim_end);

    // Both nodes fire at t=0 → TX start times coincide exactly.
    scheduler.add_node(
        Box::new(AlohaNode::new(
            NodeId(1),
            FireOnce::new(vec![0x01], 0),
            0,
            10,
        )),
        Some(0),
    );
    scheduler.add_node(
        Box::new(AlohaNode::new(
            NodeId(2),
            FireOnce::new(vec![0x02], 0),
            0,
            20,
        )),
        Some(0),
    );
    scheduler.add_node(Box::new(Receiver::new(3)), None);

    scheduler.run();

    assert_eq!(scheduler.metrics.total_tx, 2, "both nodes must transmit");
    assert!(
        scheduler.metrics.total_collisions >= 2,
        "both concurrent same-SF/freq TXs must collide; got {}",
        scheduler.metrics.total_collisions
    );
    assert_eq!(
        scheduler.metrics.total_rx, 0,
        "no frame must be delivered when both collide"
    );
}

// ---------------------------------------------------------------------------
// Test 3: backoff_retransmission
// ---------------------------------------------------------------------------

/// A node with two queued payloads and a non-zero backoff window must place
/// its second TX strictly after the first TX, and that gap must be at least
/// the on-air duration of the first packet.
#[test]
fn backoff_retransmission() {
    // Backoff window: up to 200 ms.
    let backoff_range_us = 200_000u64;
    // Give enough time for 2 TXs + max backoff.
    let sim_end = LORA_SF7_DURATION_US * 2 + backoff_range_us + 100_000;

    let tx_times: Rc<RefCell<Vec<SimTime>>> = Rc::new(RefCell::new(Vec::new()));
    let tx_times_out = Rc::clone(&tx_times);

    let mut scheduler = Scheduler::new(sim_end);
    scheduler.add_node(
        Box::new(RecordingNode {
            inner: AlohaNode::new(NodeId(1), NShot(2), backoff_range_us, 42),
            tx_times,
        }),
        Some(0),
    );
    scheduler.add_node(Box::new(Receiver::new(2)), None);
    scheduler.run();

    let times = tx_times_out.borrow();
    assert_eq!(
        times.len(),
        2,
        "two-shot traffic must produce exactly two TXs; got {}",
        times.len()
    );
    assert!(
        times[1] > times[0],
        "second TX ({}) must happen after first TX ({})",
        times[1],
        times[0]
    );
    // The node cannot re-transmit until after the first packet has been on
    // air (TX duration) plus any backoff time (≥ 0).
    assert!(
        times[1] >= times[0] + LORA_SF7_DURATION_US,
        "second TX must be at least one TX-duration after the first; \
         first={} second={} duration={}",
        times[0],
        times[1],
        LORA_SF7_DURATION_US
    );

    // Sanity: total_tx matches.
    assert_eq!(scheduler.metrics.total_tx, 2);
}
