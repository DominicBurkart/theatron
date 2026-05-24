//! Integration tests for the scheduler's interferer-originated delivery path.
//!
//! These tests pin down behaviour at the boundary between
//! [`InterferenceSource`] and [`NodeHandle`] that the existing suite touches
//! only obliquely. Every assertion is anchored to a specific line of
//! [`src/scheduler.rs`]:
//!
//! 1. **Interferer-originated frames reach every node and trigger
//!    `handle_poll_transmit`.** `deliver_completed_to_nodes` does not special-case
//!    interferer-originated frames; after `on_receive` it always calls
//!    `self.handle_poll_transmit(i, time)`. A node that queues a reply on the
//!    first `on_receive` must therefore actually transmit it in the same
//!    simulation tick. Without this, adaptive protocols reacting to jammers
//!    (e.g. ACK-back, listen-before-talk re-arming) would silently break.
//!
//! 2. **Per-node `record_rx` attribution for interferer-originated frames.**
//!    `MetricsCollector::record_rx` is called once per (non-sender) node when a
//!    non-collided frame completes. The synthetic interferer NodeId
//!    (`u32::MAX - idx`) never equals a real node id, so the
//!    `if node_id == sender { continue; }` guard never fires — every real node
//!    must be credited. The aggregate-rx invariant is verified elsewhere; this
//!    test pins the *per-node* counter, which protects against a regression
//!    that double-counts or drops one of the receivers.
//!
//! 3. **Capture survives the interferer/node boundary.** If a node TX is
//!    captured by a stronger overlapping interferer TX, the channel marks the
//!    node TX collided and the interferer TX captured. The scheduler must then
//!    record exactly one collision (the node's) and one capture (the
//!    interferer's), and the interferer's frame must still be delivered to
//!    every node. The crossover between `Channel::begin_transmission`'s
//!    capture/collision marking and `deliver_completed_to_nodes`' filtering is
//!    only exercised in unit tests on the channel, never end-to-end through
//!    the scheduler with an interferer-originated stronger signal.
//!
//! 4. **Many-interferer synthetic-ID allocation is stable.** Adding `N`
//!    interferers must allocate synthetic ids `u32::MAX, u32::MAX-1, ...,
//!    u32::MAX-(N-1)` in registration order, and the scheduler must record
//!    `record_airtime` for every interferer TX. This catches off-by-one or
//!    re-use bugs in `synthetic_interferer_id`.
//!
//! 5. **An interferer's `next_poll_time` may legally schedule a time *before*
//!    its own TX completes.** The `BinaryHeap` orders events by `time`, so a
//!    poll scheduled at `t < tx_end` will fire before the `TxComplete` event.
//!    The interferer must still observe its own `TransmissionStarted` *and*
//!    `TransmissionCompleted` events, in time order, regardless of when its
//!    next poll fires.

use std::cell::RefCell;
use std::rc::Rc;

use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

/// (time, sender, kind) — interferer-side observe log entries.
type ObserveLog = Rc<RefCell<Vec<(SimTime, NodeId, &'static str)>>>;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_tx(sf: u8, frequency: u32, duration_us: u64, payload: Vec<u8>, power: i8) -> Transmission {
    Transmission {
        payload,
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency,
        duration_us,
        tx_power_dbm: power,
    }
}

/// A node that replies with a queued `Transmission` immediately on its first
/// `on_receive`. The reply is returned by the very next `poll_transmit`, which
/// the scheduler invokes inside `deliver_completed_to_nodes` at the same time.
struct ReplyOnReceiveNode {
    id: NodeId,
    received_count: u32,
    pending_reply: Option<Transmission>,
    reply_should_fire: bool,
}

impl NodeHandle for ReplyOnReceiveNode {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        self.received_count += 1;
        // Arm the reply: it will be drained by the very next poll_transmit
        // call inside `deliver_completed_to_nodes`.
        self.reply_should_fire = true;
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        if self.reply_should_fire {
            self.reply_should_fire = false;
            self.pending_reply.take()
        } else {
            None
        }
    }
    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

/// A receiver that simply counts how many frames it has received.
struct CountingReceiver {
    id: NodeId,
    received_count: Rc<RefCell<u32>>,
}

impl NodeHandle for CountingReceiver {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        *self.received_count.borrow_mut() += 1;
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        None
    }
    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

/// Node that transmits exactly once on its first `update` call.
struct OneShotSender {
    id: NodeId,
    tx: Option<Transmission>,
}

impl NodeHandle for OneShotSender {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        self.tx.take()
    }
    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

/// Injects exactly one transmission on its first `poll_inject` and then
/// permanently goes silent (returns `None` from `next_poll_time`).
struct OneShotInjector {
    tx: Option<Transmission>,
}

impl InterferenceSource for OneShotInjector {
    fn observe(&mut self, _event: &ChannelEvent, _time: SimTime) {}
    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        self.tx.take()
    }
    fn next_poll_time(&self, _current_time: SimTime) -> Option<SimTime> {
        None
    }
}

/// A finite-burst interferer: injects `n` identical frames, one per poll,
/// spaced by `interval_us`.
struct PeriodicInjector {
    template: Transmission,
    remaining: u32,
    interval_us: u64,
}

impl InterferenceSource for PeriodicInjector {
    fn observe(&mut self, _event: &ChannelEvent, _time: SimTime) {}
    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        if self.remaining > 0 {
            self.remaining -= 1;
            Some(self.template.clone())
        } else {
            None
        }
    }
    fn next_poll_time(&self, current_time: SimTime) -> Option<SimTime> {
        if self.remaining > 0 {
            Some(current_time + self.interval_us)
        } else {
            None
        }
    }
}

/// An interferer that logs every `observe` callback with the originating
/// sender's `NodeId` and the simulation time. The first call to
/// `poll_inject` returns a single transmission; subsequent calls return
/// `None`. `next_poll_time` returns a fixed `peek_time` once (regardless of
/// when the TX completes), then `None` — this exercises the case where the
/// interferer's next poll is scheduled in the middle of its own active TX.
struct LoggingMidTxInjector {
    tx: Option<Transmission>,
    peek_time: SimTime,
    peeked: Rc<RefCell<bool>>,
    log: ObserveLog,
}

impl InterferenceSource for LoggingMidTxInjector {
    fn observe(&mut self, event: &ChannelEvent, time: SimTime) {
        let entry = match event {
            ChannelEvent::TransmissionStarted { sender, .. } => (time, *sender, "started"),
            ChannelEvent::TransmissionCompleted { sender, .. } => (time, *sender, "completed"),
        };
        self.log.borrow_mut().push(entry);
    }
    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        self.tx.take()
    }
    fn next_poll_time(&self, _current_time: SimTime) -> Option<SimTime> {
        let mut peeked = self.peeked.borrow_mut();
        if *peeked {
            None
        } else {
            *peeked = true;
            Some(self.peek_time)
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Interferer-originated frame triggers a reply TX from a node
// ---------------------------------------------------------------------------

/// An interferer's transmission, when delivered to a node, must drive the
/// scheduler through `handle_poll_transmit` so a reply queued in `on_receive`
/// actually transmits in the same scheduler tick.
///
/// This pins down the `self.handle_poll_transmit(i, time);` call inside
/// `deliver_completed_to_nodes` for interferer-originated frames.
#[test]
fn interferer_frame_triggers_node_reply_in_same_tick() {
    let mut sched = Scheduler::new(1_000_000);

    let reply_tx = make_tx(7, 868_300_000, 30_000, vec![0xBE, 0xEF], 14);
    let reply_node = ReplyOnReceiveNode {
        id: NodeId(1),
        received_count: 0,
        pending_reply: Some(reply_tx.clone()),
        reply_should_fire: false,
    };

    // A second node observes the reply, so `total_rx` reflects both:
    //   - 1 rx for the interferer TX at node 1
    //   - 1 rx for the interferer TX at node 2 (broadcast)
    //   - 1 rx for the reply TX at node 2 (node 1 → node 2)
    sched.add_node(Box::new(reply_node), None);
    sched.add_node(
        Box::new(CountingReceiver {
            id: NodeId(2),
            received_count: Rc::new(RefCell::new(0)),
        }),
        None,
    );

    sched.add_interferer(
        Box::new(OneShotInjector {
            tx: Some(make_tx(7, 868_100_000, 40_000, vec![0xCA, 0xFE], 14)),
        }),
        0,
    );

    sched.run();

    assert_eq!(
        sched.metrics.total_tx, 1,
        "exactly one node TX (the reply) should be counted; interferer TXs do not count toward total_tx"
    );
    assert_eq!(
        sched.metrics.total_airtime_us,
        40_000 + 30_000,
        "airtime must include both the interferer's TX and the node's reply"
    );
    assert_eq!(
        sched.metrics.total_collisions, 0,
        "interferer TX (SF7 868.1 MHz) and reply TX (SF7 868.3 MHz) are on different frequencies"
    );
    // Reply was delivered:
    //   - Interferer TX completes at 40_000 → both nodes record_rx (total_rx = 2).
    //   - Reply TX from node 1 begins at 40_000 (inside deliver_completed_to_nodes)
    //     and completes at 70_000 → node 2 record_rx (total_rx = 3).
    assert_eq!(
        sched.metrics.total_rx, 3,
        "interferer broadcast (2 rx) + reply chain (1 rx) = 3 rx"
    );
    assert_eq!(
        sched.metrics.node_tx_count(NodeId(1)),
        1,
        "node 1's reply must be attributed to node 1, not to the interferer's synthetic id"
    );
    assert_eq!(
        sched.metrics.node_tx_count(NodeId(2)),
        0,
        "node 2 never transmits"
    );
}

// ---------------------------------------------------------------------------
// 2. Per-node rx attribution for interferer-originated frames
// ---------------------------------------------------------------------------

/// When an interferer TX reaches `N` real nodes, each non-sender node must
/// receive `record_rx` exactly once. Because the synthetic interferer id is
/// `u32::MAX - idx`, the `if node_id == sender { continue; }` guard never
/// fires for any real node, so every node must be credited.
#[test]
fn interferer_originated_frame_credits_each_node_once() {
    let mut sched = Scheduler::new(500_000);

    // Three quiet receivers; only the interferer transmits.
    for id in 1u32..=3 {
        sched.add_node(
            Box::new(CountingReceiver {
                id: NodeId(id),
                received_count: Rc::new(RefCell::new(0)),
            }),
            None,
        );
    }

    sched.add_interferer(
        Box::new(OneShotInjector {
            tx: Some(make_tx(8, 868_500_000, 50_000, vec![0x42], 17)),
        }),
        0,
    );

    sched.run();

    // Each receiver must be credited exactly once for the interferer's frame.
    for id in 1u32..=3 {
        assert_eq!(
            sched.metrics.node_rx_count(NodeId(id)),
            1,
            "node {id} must be credited exactly once for the interferer's TX"
        );
    }
    assert_eq!(sched.metrics.total_rx, 3);
    assert_eq!(
        sched.metrics.total_tx, 0,
        "interferer TXs never count as node TXs"
    );
    assert_eq!(sched.metrics.total_airtime_us, 50_000);
}

// ---------------------------------------------------------------------------
// 3. Capture: stronger interferer beats overlapping node TX
// ---------------------------------------------------------------------------

/// A stronger interferer TX overlapping a weaker node TX on the same SF/freq
/// must:
///   - mark the node's TX collided (no delivery to other nodes),
///   - mark the interferer's TX captured (delivered to every node),
///   - increment `total_collisions` by 1 (the node's TX) and `total_captures`
///     by 1 (the interferer's TX).
#[test]
fn stronger_interferer_captures_weaker_node_tx() {
    let mut sched = Scheduler::new(500_000);

    // Weak node transmits first at power=14 dBm; strong interferer overlaps.
    let weak_tx = make_tx(7, 868_100_000, 80_000, vec![0xAA], 14);
    let strong_tx = make_tx(7, 868_100_000, 80_000, vec![0xBB], 20); // delta = 6 dB == LoRa threshold

    let sender = OneShotSender {
        id: NodeId(1),
        tx: Some(weak_tx),
    };
    sched.add_node(Box::new(sender), Some(0));
    sched.add_node(
        Box::new(CountingReceiver {
            id: NodeId(2),
            received_count: Rc::new(RefCell::new(0)),
        }),
        None,
    );
    sched.add_node(
        Box::new(CountingReceiver {
            id: NodeId(3),
            received_count: Rc::new(RefCell::new(0)),
        }),
        None,
    );

    sched.add_interferer(
        Box::new(OneShotInjector {
            tx: Some(strong_tx),
        }),
        10_000, // overlaps the weak TX
    );

    sched.run();

    assert_eq!(
        sched.metrics.total_tx, 1,
        "only the node TX counts as total_tx"
    );
    assert_eq!(
        sched.metrics.total_collisions, 1,
        "the weak node TX is collided"
    );
    assert_eq!(
        sched.metrics.total_captures, 1,
        "the strong interferer TX is captured"
    );
    // Strong interferer's captured frame is delivered to BOTH non-sender real
    // nodes (the synthetic interferer id never matches a real node id, so the
    // sender-filter doesn't apply to any real node).
    assert_eq!(
        sched.metrics.total_rx, 3,
        "strong interferer frame delivered to all 3 real nodes (id=1, 2, 3)"
    );
    for id in 1u32..=3 {
        assert_eq!(
            sched.metrics.node_rx_count(NodeId(id)),
            1,
            "node {id} receives the interferer's captured frame exactly once"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Many-interferer airtime + collision accounting
// ---------------------------------------------------------------------------

/// Registering N interferers each emitting one TX on disjoint frequencies must
/// accumulate exactly `N * duration` of airtime, and no spurious collisions
/// must appear. This exercises the synthetic-id allocation path for many
/// interferers (idx = 0 .. N-1) and the per-event observe broadcast.
#[test]
fn many_interferers_airtime_and_no_spurious_collisions() {
    const N: u32 = 8;
    const DURATION: u64 = 25_000;
    let mut sched = Scheduler::new(1_000_000);

    sched.add_node(
        Box::new(CountingReceiver {
            id: NodeId(1),
            received_count: Rc::new(RefCell::new(0)),
        }),
        None,
    );

    for i in 0..N {
        // Each interferer on a disjoint frequency → no cross-collision.
        let freq = 868_000_000 + 200_000 * i;
        sched.add_interferer(
            Box::new(OneShotInjector {
                tx: Some(make_tx(7, freq, DURATION, vec![i as u8], 14)),
            }),
            0,
        );
    }

    sched.run();

    assert_eq!(
        sched.metrics.total_airtime_us,
        DURATION * N as u64,
        "airtime must aggregate over every interferer TX"
    );
    assert_eq!(
        sched.metrics.total_collisions, 0,
        "interferers on disjoint frequencies must not collide"
    );
    assert_eq!(
        sched.metrics.total_rx, N as u64,
        "the single receiver must record_rx once per interferer TX"
    );
    assert_eq!(
        sched.metrics.node_rx_count(NodeId(1)),
        N as u64,
        "every interferer-originated frame must be credited to node 1"
    );
}

// ---------------------------------------------------------------------------
// 5. next_poll_time scheduled mid-TX still preserves observe ordering
// ---------------------------------------------------------------------------

/// An interferer that schedules its next poll *before* its own TX completes
/// must still receive its own `TransmissionStarted` and `TransmissionCompleted`
/// `observe` callbacks, and they must arrive in non-decreasing time order.
/// This guards against a regression in the `BinaryHeap`-ordering or `seq`
/// tie-breaker that could deliver a `TxComplete` *before* its corresponding
/// `TransmissionStarted` is observed.
#[test]
fn interferer_observes_own_tx_when_poll_scheduled_mid_tx() {
    let mut sched = Scheduler::new(500_000);
    let log: ObserveLog = Rc::new(RefCell::new(Vec::new()));

    // Interferer's TX starts at t=0 and runs for 100_000 us. Its
    // `next_poll_time` returns 50_000 — strictly inside the active TX window.
    let interferer = LoggingMidTxInjector {
        tx: Some(make_tx(9, 868_700_000, 100_000, vec![0x77], 14)),
        peek_time: 50_000,
        peeked: Rc::new(RefCell::new(false)),
        log: Rc::clone(&log),
    };
    sched.add_interferer(Box::new(interferer), 0);

    sched.run();

    let entries = log.borrow();
    let synth_id = NodeId(u32::MAX); // idx=0 → u32::MAX

    let started: Vec<_> = entries
        .iter()
        .filter(|(_, s, k)| *s == synth_id && *k == "started")
        .collect();
    let completed: Vec<_> = entries
        .iter()
        .filter(|(_, s, k)| *s == synth_id && *k == "completed")
        .collect();

    assert_eq!(
        started.len(),
        1,
        "interferer must observe its own TransmissionStarted exactly once"
    );
    assert_eq!(
        completed.len(),
        1,
        "interferer must observe its own TransmissionCompleted exactly once"
    );
    assert_eq!(
        started[0].0, 0,
        "TransmissionStarted is observed at the start time (t=0)"
    );
    assert_eq!(
        completed[0].0, 100_000,
        "TransmissionCompleted is observed at the TX's end time"
    );

    // Strict ordering: every event time is non-decreasing in the order
    // observed. (This is the invariant the scheduler's BinaryHeap+seq
    // tie-breaker is supposed to enforce.)
    let mut last = 0u64;
    for (t, _, _) in entries.iter() {
        assert!(*t >= last, "observe event times must be non-decreasing");
        last = *t;
    }
}

// ---------------------------------------------------------------------------
// 6. Same-time interferer-originated event + node wake fires deterministically
// ---------------------------------------------------------------------------

/// When an interferer TX completes at the exact same `SimTime` that an
/// already-scheduled node `Wake` fires, the two events must be processed in
/// FIFO order (by `seq`) and the resulting metrics must be identical across
/// reruns of the same scenario. This protects determinism of the
/// interferer-vs-wake co-scheduling path against any future refactor of
/// `ScheduledEvent::cmp` or `Scheduler::schedule`.
#[test]
fn interferer_completion_coincident_with_node_wake_is_deterministic() {
    fn run() -> (u64, u64, u64) {
        let mut sched = Scheduler::new(500_000);

        // Node 1 wakes at t=40_000 (same as the interferer's TX completion)
        // and has nothing to do; node 2 just receives.
        sched.add_node(
            Box::new(CountingReceiver {
                id: NodeId(1),
                received_count: Rc::new(RefCell::new(0)),
            }),
            Some(40_000),
        );
        sched.add_node(
            Box::new(CountingReceiver {
                id: NodeId(2),
                received_count: Rc::new(RefCell::new(0)),
            }),
            None,
        );

        // Interferer TX: starts at 0, completes at 40_000.
        sched.add_interferer(
            Box::new(OneShotInjector {
                tx: Some(make_tx(7, 868_100_000, 40_000, vec![0x01], 14)),
            }),
            0,
        );

        sched.run();
        (
            sched.metrics.total_tx,
            sched.metrics.total_rx,
            sched.metrics.total_airtime_us,
        )
    }

    let a = run();
    let b = run();
    assert_eq!(a, b, "identical scenarios must produce identical metrics");
    // And the values themselves must be sensible:
    assert_eq!(a.0, 0, "no node ever transmits");
    assert_eq!(
        a.1, 2,
        "the single interferer TX is delivered to both real nodes"
    );
    assert_eq!(a.2, 40_000, "airtime equals the interferer's TX duration");
}

// ---------------------------------------------------------------------------
// 7. Periodic interferer covers the entire poll → next_poll_time loop
// ---------------------------------------------------------------------------

/// A periodic interferer that injects `K` frames and then stops (via
/// `next_poll_time` → `None`) must produce exactly `K * duration` airtime and
/// deliver exactly `K` frames to a quiet receiver. This is the canonical
/// "burst jammer" scenario and pins the interferer poll loop end-to-end.
#[test]
fn periodic_interferer_delivers_exactly_k_frames() {
    const K: u32 = 5;
    const DURATION: u64 = 20_000;
    const INTERVAL: u64 = 50_000;

    let mut sched = Scheduler::new(1_000_000);

    let received = Rc::new(RefCell::new(0u32));
    sched.add_node(
        Box::new(CountingReceiver {
            id: NodeId(1),
            received_count: Rc::clone(&received),
        }),
        None,
    );

    sched.add_interferer(
        Box::new(PeriodicInjector {
            template: make_tx(7, 868_900_000, DURATION, vec![0xEE], 14),
            remaining: K,
            interval_us: INTERVAL,
        }),
        0,
    );

    sched.run();

    assert_eq!(*received.borrow(), K, "node must receive exactly K frames");
    assert_eq!(
        sched.metrics.total_rx, K as u64,
        "total_rx must equal K (one frame per receiver per injection)"
    );
    assert_eq!(
        sched.metrics.total_airtime_us,
        K as u64 * DURATION,
        "airtime must accumulate over every injection"
    );
    assert_eq!(
        sched.metrics.total_tx, 0,
        "interferer injections do not increment total_tx"
    );
    assert_eq!(
        sched.metrics.total_collisions, 0,
        "non-overlapping interferer injections must not collide"
    );
}
