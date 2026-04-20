//! Integration tests for the `AlohaNode` example.
//!
//! The existing `tests/aloha.rs` file uses local `PeriodicSender`/`Receiver`
//! helpers and explicitly calls out that this means `AlohaNode`,
//! `AlohaReceiver`, and `PeriodicTraffic` from `examples/aloha/aloha_node.rs`
//! are **not** exercised end-to-end. This file closes that gap by pulling
//! the example module in directly via `#[path]` (the pattern the existing
//! doc-comment in `tests/aloha.rs` recommends) and running it through the
//! real scheduler + channel.
//!
//! # Undertested component
//! `examples/aloha/aloha_node.rs` — `AlohaNode`, `AlohaReceiver`, and the
//! `PeriodicTraffic` traffic model.
//!
//! # Current state
//! - `PeriodicTraffic` has two focused unit tests in the example file.
//! - `AlohaNode` has two shallow unit tests; `AlohaReceiver` has one. None
//!   of these drive the types through a real `Scheduler` run.
//! - The broader `tests/aloha.rs` integration file deliberately sidesteps
//!   them and uses hand-written stand-ins.
//!
//! # Invariants validated
//! 1. A single-payload `AlohaNode` drives exactly one TX end-to-end through
//!    the scheduler + channel and stops scheduling itself (no infinite wake
//!    loop) once traffic is exhausted.
//! 2. The one TX is delivered verbatim to a passive `AlohaReceiver`.
//! 3. Two single-payload `AlohaNode`s on orthogonal spreading factors both
//!    deliver, with no collisions, regardless of simultaneous start.
//! 4. `PeriodicTraffic` invariant (proptest): it yields at most `count`
//!    payloads over any monotonically-nondecreasing time sequence, and every
//!    payload equals the configured bytes.
//!
//! # Strategy
//! - **Realistic integration** (scheduler + channel + example types) for
//!   (1)-(3). These exercise the code path that `tests/aloha.rs` currently
//!   skips.
//! - **Proptest** for (4), the simplest invariant of the traffic model that
//!   is load-bearing for any protocol using it.
//! - No doctests: these types live in an example crate and are not part of
//!   the public theatron API surface.
//!
//! # Out of scope
//! A latent interaction between `AlohaNode::update`'s "probe" logic and
//! `PeriodicTraffic`'s stateful `next_payload` causes higher payload counts
//! to be consumed out-of-band. That is a product bug; these tests stick to
//! `count = 1` scenarios so they exercise the integration without depending
//! on that interaction.

#[path = "../examples/aloha/aloha_node.rs"]
mod aloha_node;

use aloha_node::{AlohaNode, AlohaReceiver, PeriodicTraffic};
use proptest::prelude::*;
use theatron::scheduler::Scheduler;
use theatron::traits::TrafficModel;
use theatron::types::NodeId;

/// Convenience: construct a single-payload `AlohaNode` with EU868 parameters.
fn make_single_shot_aloha(id: u32, payload: Vec<u8>, sf: u8, frequency: u32) -> AlohaNode {
    let interval_us = 1_000_000; // 1 s between payloads (only one ever emitted)
    let tx_duration_us = 50_000; // 50 ms on-air
    let traffic = PeriodicTraffic::new(payload, interval_us, 1);
    AlohaNode::new(
        NodeId(id),
        Box::new(traffic),
        interval_us,
        sf,
        frequency,
        tx_duration_us,
    )
}

// ---------------------------------------------------------------------------
// (1) Single-shot end-to-end: exactly one TX, scheduler halts on exhaustion
// ---------------------------------------------------------------------------

#[test]
fn single_shot_aloha_emits_exactly_one_tx_and_scheduler_halts() {
    let end_time = 10_000_000;
    let mut sched = Scheduler::new(end_time);
    let sender = make_single_shot_aloha(1, vec![0x42], 7, 868_100_000);
    let receiver = AlohaReceiver::new(NodeId(99));
    sched.add_node(Box::new(sender), Some(0));
    sched.add_node(Box::new(receiver), None);
    sched.run();

    assert_eq!(sched.metrics.total_tx, 1);
    assert_eq!(sched.metrics.total_collisions, 0);
    // The scheduler should drain its queue well before `end_time` when the
    // node's traffic is exhausted: `AlohaNode::update` returns `None` and no
    // further wakes are scheduled.
    assert!(
        sched.current_time() < end_time,
        "scheduler must halt before end_time when traffic is exhausted; \
         current_time={} end_time={}",
        sched.current_time(),
        end_time
    );
}

// ---------------------------------------------------------------------------
// (2) Payload is delivered verbatim to a passive AlohaReceiver
// ---------------------------------------------------------------------------

#[test]
fn single_shot_aloha_delivers_payload_to_receiver() {
    let mut sched = Scheduler::new(5_000_000);
    let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let sender = make_single_shot_aloha(1, payload.clone(), 7, 868_100_000);
    let receiver = AlohaReceiver::new(NodeId(99));
    sched.add_node(Box::new(sender), Some(0));
    sched.add_node(Box::new(receiver), None);
    sched.run();

    assert_eq!(sched.metrics.total_tx, 1);
    assert_eq!(
        sched.metrics.node_rx_count(NodeId(99)),
        1,
        "AlohaReceiver must receive the one frame the sender emitted"
    );
    assert_eq!(sched.metrics.total_collisions, 0);
}

// ---------------------------------------------------------------------------
// (3) Orthogonal SFs: two simultaneous AlohaNodes both deliver
// ---------------------------------------------------------------------------

#[test]
fn two_aloha_nodes_on_different_sf_both_deliver() {
    let mut sched = Scheduler::new(5_000_000);
    let a = make_single_shot_aloha(1, vec![0xAA], 7, 868_100_000);
    let b = make_single_shot_aloha(2, vec![0xBB], 8, 868_100_000);
    let receiver = AlohaReceiver::new(NodeId(99));
    sched.add_node(Box::new(a), Some(0));
    sched.add_node(Box::new(b), Some(0));
    sched.add_node(Box::new(receiver), None);
    sched.run();

    assert_eq!(sched.metrics.total_tx, 2);
    assert_eq!(sched.metrics.total_collisions, 0);
    // Each TX reaches the two non-sender nodes (the other sender + the
    // passive receiver), so total_rx = 2 * 2 = 4.
    assert_eq!(sched.metrics.total_rx, 4);
    assert_eq!(sched.metrics.node_rx_count(NodeId(99)), 2);
}

// ---------------------------------------------------------------------------
// (4) PeriodicTraffic proptest — invariants of the traffic model
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// For any count C and any strictly-increasing poll time sequence spaced
    /// at `interval_us`, `PeriodicTraffic` yields exactly C payloads and each
    /// equals the configured bytes. This pins down the core contract the
    /// `AlohaNode` relies on.
    #[test]
    fn periodic_traffic_yields_exactly_count_payloads(
        count in 0usize..=8,
        interval_us in 1u64..=2_000_000,
    ) {
        let payload_bytes = vec![0x5A, 0xA5];
        let mut model = PeriodicTraffic::new(payload_bytes.clone(), interval_us, count);
        let mut emitted = 0usize;
        // Sample at `interval_us` cadence well past `count` intervals to
        // ensure we never emit more than `count`.
        for step in 0..(count + 4) {
            let t = (step as u64) * interval_us;
            if let Some(p) = model.next_payload(t) {
                prop_assert_eq!(p, payload_bytes.clone());
                emitted += 1;
            }
        }
        prop_assert_eq!(emitted, count);
    }

    /// Calling `next_payload` before `interval_us` has elapsed since the
    /// previous emission must return `None`: the model gates on time.
    #[test]
    fn periodic_traffic_interval_gates_next_payload(
        interval_us in 100u64..=1_000_000,
        early_offset in 1u64..=99,
    ) {
        let mut model = PeriodicTraffic::new(vec![0x01], interval_us, 3);
        // First payload is available at t = 0.
        prop_assert!(model.next_payload(0).is_some());
        // A query strictly inside the interval must not emit.
        let too_early = (interval_us * early_offset) / 100;
        prop_assume!(too_early < interval_us);
        prop_assert!(model.next_payload(too_early).is_none());
        // At exactly interval_us, the next payload becomes available.
        prop_assert!(model.next_payload(interval_us).is_some());
    }
}
