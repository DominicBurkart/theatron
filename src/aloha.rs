//! Pure ALOHA reference MAC protocol.
//!
//! Nodes transmit immediately whenever the traffic model produces a payload
//! (no carrier sensing, no time-slotting).  After every transmission a
//! uniformly-distributed random backoff is drawn before the next traffic-model
//! poll, preventing perfectly correlated retransmissions.
//!
//! # Design notes
//!
//! The node is driven entirely through the [`NodeHandle`] interface:
//!
//! 1. The scheduler calls `update(time)` on each wake event.
//! 2. `update` queries the traffic model and, when a payload is available,
//!    stores it in `pending_payload` and returns `Some(time)` (immediate
//!    re-wake) so the scheduler calls `poll_transmit` right away.
//! 3. `poll_transmit` hands the payload to the scheduler and records a
//!    pending backoff duration.  Because `poll_transmit` does not receive the
//!    current time, the *absolute* backoff deadline is computed in the next
//!    `update` call once the post-TX time is known.
//! 4. On re-wake after the TX is initiated, `update` sees `pending_backoff_us`,
//!    computes `until = time + LORA_SF7_DURATION_US + pending_backoff_us`
//!    (ensuring the backoff guard covers the full on-air time), enters
//!    `Backoff { until }`, and returns `Some(until)` so the scheduler wakes
//!    the node at the right moment.
//!
//! # Example
//!
//! ```
//! use theatron::aloha::{AlohaNode, PoissonTraffic};
//! use theatron::scheduler::{NodeHandle, Scheduler};
//! use theatron::types::NodeId;
//!
//! let traffic = PoissonTraffic::new(10_000_000, 0xDEAD_BEEF);
//! let node = AlohaNode::new(NodeId(1), traffic, 2_000_000, 0xCAFE_BABE);
//! let mut scheduler = Scheduler::new(10_000_000);
//! scheduler.add_node(Box::new(node), Some(0));
//! scheduler.run();
//! ```

use crate::scheduler::NodeHandle;
use crate::time::SimTime;
use crate::traits::TrafficModel;
use crate::types::{NodeId, RxMetadata, Transmission};

/// LoRa SF7 / BW125 time-on-air for a minimal payload (~56 ms).
pub const LORA_SF7_DURATION_US: u64 = 56_000;
/// EU868 primary uplink frequency (Hz).
pub const LORA_FREQUENCY: u32 = 868_100_000;
/// Default TX power (dBm).
pub const LORA_TX_POWER_DBM: i8 = 14;

// ---------------------------------------------------------------------------
// Minimal self-contained xorshift64 PRNG — no external crate needed.
// ---------------------------------------------------------------------------

/// A fast, deterministic xorshift64 PRNG.
///
/// Used by both [`PoissonTraffic`] and [`AlohaNode`] so the module requires no
/// dependencies beyond `std`.
///
/// # Example
///
/// ```
/// use theatron::aloha::Xorshift64;
/// let mut rng = Xorshift64::new(42);
/// let a = rng.next_u64();
/// let b = rng.next_u64();
/// assert_ne!(a, b);
/// ```
#[derive(Clone)]
pub struct Xorshift64(u64);

impl Xorshift64 {
    /// Create a new RNG.  A seed of `0` is replaced by `1` to avoid the
    /// all-zero fixed point.
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 { 1 } else { seed })
    }

    /// Return the next pseudo-random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Return a value uniformly distributed in `[0, range)`.
    ///
    /// Uses rejection sampling to eliminate modulo bias.
    pub fn next_u64_below(&mut self, range: u64) -> u64 {
        if range <= 1 {
            return 0;
        }
        let threshold = u64::MAX - (u64::MAX % range);
        loop {
            let v = self.next_u64();
            if v < threshold {
                return v % range;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PoissonTraffic
// ---------------------------------------------------------------------------

/// A [`TrafficModel`] that approximates Poisson packet arrivals.
///
/// Inter-arrival times are drawn from a uniform distribution centred on
/// `mean_us` — i.e. `U(mean_us/2, 3·mean_us/2)` — giving the correct mean
/// while staying entirely in integer arithmetic.
///
/// # Example
///
/// ```
/// use theatron::aloha::PoissonTraffic;
/// use theatron::traits::TrafficModel;
///
/// let mut traffic = PoissonTraffic::new(1_000_000, 42);
/// // First call at time 0 schedules the first arrival; returns None.
/// assert!(traffic.next_payload(0).is_none());
/// ```
pub struct PoissonTraffic {
    mean_us: u64,
    rng: Xorshift64,
    /// Absolute simulation time at which the next packet arrives.
    next_arrival_us: Option<SimTime>,
}

impl PoissonTraffic {
    /// Create a new traffic model.
    ///
    /// * `mean_us` — mean inter-arrival time in microseconds.
    /// * `seed`    — RNG seed for reproducibility.
    pub fn new(mean_us: u64, seed: u64) -> Self {
        Self {
            mean_us,
            rng: Xorshift64::new(seed),
            next_arrival_us: None,
        }
    }

    /// Draw the next inter-arrival interval from `U(mean/2, 3·mean/2)`.
    fn draw_interval(&mut self) -> u64 {
        let half = self.mean_us / 2;
        // range = mean_us so the window spans [half, half + mean_us) = [mean/2, 3·mean/2).
        let range = self.mean_us.max(1);
        half + self.rng.next_u64_below(range)
    }
}

impl TrafficModel for PoissonTraffic {
    fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>> {
        match self.next_arrival_us {
            None => {
                // First poll: schedule the first arrival relative to now.
                let interval = self.draw_interval();
                self.next_arrival_us = Some(time + interval);
                None
            }
            Some(next) if time >= next => {
                // Arrival ready — schedule the next one and return a payload.
                let interval = self.draw_interval();
                self.next_arrival_us = Some(time + interval);
                Some(vec![0x01])
            }
            Some(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// AlohaNode state machine
// ---------------------------------------------------------------------------

/// Internal phase of an [`AlohaNode`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Polling the traffic model on each wake.
    Idle,
    /// Waiting until `until` µs before polling again.
    Backoff { until: SimTime },
    /// A payload is staged; waiting for `poll_transmit` to collect it.
    ReadyToTransmit,
    /// `poll_transmit` has been called and the TX is on air; the *next*
    /// `update` call will compute the absolute backoff deadline from the
    /// stored relative duration plus the on-air guard time.
    AwaitingBackoffStart { duration_us: u64 },
}

/// A Pure ALOHA node that implements [`NodeHandle`].
///
/// # Transmit flow
///
/// ```text
/// Idle ──(payload ready)──► ReadyToTransmit
///   ▲                             │
///   │                     poll_transmit() called
///   │                             │
///   │                   AwaitingBackoffStart { duration_us }
///   │                             │
///   │                     next update(time) call
///   │                             │
///   │         Backoff { until = time + LORA_SF7_DURATION_US + duration_us }
///   │                             │
///   └──────(until reached)────────┘
/// ```
///
/// # Example
///
/// ```
/// use theatron::aloha::{AlohaNode, PoissonTraffic};
/// use theatron::scheduler::NodeHandle;
/// use theatron::types::NodeId;
///
/// let traffic = PoissonTraffic::new(5_000_000, 1);
/// let mut node = AlohaNode::new(NodeId(7), traffic, 500_000, 99);
/// // Node starts in Idle; update at t=0 schedules first traffic poll.
/// let next = node.update(0);
/// assert!(next.is_some());
/// ```
pub struct AlohaNode<T: TrafficModel> {
    id: NodeId,
    traffic: T,
    /// Upper bound of the uniform backoff window (µs).
    backoff_range_us: u64,
    rng: Xorshift64,
    phase: Phase,
    /// Staged payload waiting to be collected by `poll_transmit`.
    pending_payload: Option<Vec<u8>>,
    /// How often (µs) to poll the traffic model when idle.
    poll_interval_us: u64,
}

impl<T: TrafficModel> AlohaNode<T> {
    /// Create a new Pure ALOHA node.
    ///
    /// * `id`               — unique node identifier.
    /// * `traffic`          — traffic model producing payloads.
    /// * `backoff_range_us` — backoff drawn from `U(0, backoff_range_us)`.
    /// * `seed`             — RNG seed (for the backoff RNG).
    pub fn new(id: NodeId, traffic: T, backoff_range_us: u64, seed: u64) -> Self {
        Self {
            id,
            traffic,
            backoff_range_us,
            rng: Xorshift64::new(seed),
            phase: Phase::Idle,
            pending_payload: None,
            poll_interval_us: 1_000, // 1 ms idle polling granularity
        }
    }

    fn draw_backoff(&mut self) -> u64 {
        self.rng.next_u64_below(self.backoff_range_us.max(1))
    }
}

impl<T: TrafficModel> NodeHandle for AlohaNode<T> {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        match self.phase.clone() {
            // ── Backoff just started; now we know the current time. ──────────
            //
            // Add the on-air guard (LORA_SF7_DURATION_US) so the node always
            // waits at least one full TX duration before re-attempting,
            // regardless of the drawn backoff value.
            Phase::AwaitingBackoffStart { duration_us } => {
                let until = time + LORA_SF7_DURATION_US + duration_us;
                self.phase = Phase::Backoff { until };
                return Some(until);
            }

            // ── Waiting for backoff to expire. ───────────────────────────────
            Phase::Backoff { until } => {
                if time < until {
                    return Some(until); // still waiting
                }
                // Expired — fall through to idle poll below.
                self.phase = Phase::Idle;
            }

            // ── Payload staged; scheduler is about to call poll_transmit. ────
            // Return the same time so poll_transmit is invoked immediately.
            Phase::ReadyToTransmit => {
                return Some(time);
            }

            Phase::Idle => {}
        }

        // Idle: ask the traffic model for the next payload.
        match self.traffic.next_payload(time) {
            Some(payload) => {
                self.pending_payload = Some(payload);
                self.phase = Phase::ReadyToTransmit;
                Some(time) // re-wake immediately so poll_transmit is called
            }
            None => Some(time + self.poll_interval_us),
        }
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        if self.phase != Phase::ReadyToTransmit {
            return None;
        }
        let payload = self.pending_payload.take()?;
        let backoff = self.draw_backoff();
        // We don't have the current time here; record the relative duration
        // so `update` can compute the absolute deadline on its next call.
        self.phase = Phase::AwaitingBackoffStart {
            duration_us: backoff,
        };
        Some(Transmission {
            payload,
            sf: 7,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency: LORA_FREQUENCY,
            duration_us: LORA_SF7_DURATION_US,
            tx_power_dbm: LORA_TX_POWER_DBM,
        })
    }

    /// Pure ALOHA has no carrier sense; received frames do not affect the TX
    /// schedule.
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Xorshift64
    // ------------------------------------------------------------------

    #[test]
    fn xorshift_zero_seed_becomes_one() {
        let mut a = Xorshift64::new(0);
        let mut b = Xorshift64::new(1);
        assert_eq!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn xorshift_deterministic() {
        let mut a = Xorshift64::new(42);
        let mut b = Xorshift64::new(42);
        for _ in 0..200 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn xorshift_nonzero_output() {
        let mut rng = Xorshift64::new(1);
        for _ in 0..1_000 {
            assert_ne!(rng.next_u64(), 0);
        }
    }

    #[test]
    fn xorshift_below_range_in_bounds() {
        let mut rng = Xorshift64::new(7);
        let range = 100u64;
        for _ in 0..10_000 {
            assert!(rng.next_u64_below(range) < range);
        }
    }

    #[test]
    fn xorshift_below_zero_range_returns_zero() {
        let mut rng = Xorshift64::new(3);
        assert_eq!(rng.next_u64_below(0), 0);
        assert_eq!(rng.next_u64_below(1), 0);
    }

    // ------------------------------------------------------------------
    // PoissonTraffic
    // ------------------------------------------------------------------

    #[test]
    fn poisson_first_call_returns_none() {
        let mut t = PoissonTraffic::new(1_000_000, 42);
        assert!(t.next_payload(0).is_none());
    }

    #[test]
    fn poisson_eventually_fires() {
        let mut t = PoissonTraffic::new(1_000, 42);
        // Advance in 1 µs steps; a packet must arrive within 2*mean = 2000 µs.
        let fired = (0u64..2_000).any(|time| t.next_payload(time).is_some());
        assert!(fired, "traffic model must eventually produce a payload");
    }

    #[test]
    fn poisson_second_packet_after_first() {
        let mut t = PoissonTraffic::new(100, 99);
        // Skip until first arrival.
        let first = (0u64..1_000).find(|&time| t.next_payload(time).is_some());
        let first = first.expect("first packet must arrive");
        // Second packet must arrive later.
        let second = (first + 1..first + 1_000).find(|&time| t.next_payload(time).is_some());
        assert!(second.is_some(), "second packet must arrive after first");
        assert!(second.unwrap() > first);
    }

    // ------------------------------------------------------------------
    // AlohaNode
    // ------------------------------------------------------------------

    struct OneShot(Option<Vec<u8>>);
    impl TrafficModel for OneShot {
        fn next_payload(&mut self, _time: SimTime) -> Option<Vec<u8>> {
            self.0.take()
        }
    }

    struct NeverFire;
    impl TrafficModel for NeverFire {
        fn next_payload(&mut self, _time: SimTime) -> Option<Vec<u8>> {
            None
        }
    }

    #[test]
    fn idle_node_keeps_waking() {
        let mut node = AlohaNode::new(NodeId(1), NeverFire, 100_000, 1);
        let w1 = node.update(0);
        assert!(w1.is_some());
        let t1 = w1.unwrap();
        assert!(t1 > 0, "should schedule a future wake");
        let w2 = node.update(t1);
        assert!(w2.is_some());
    }

    #[test]
    fn one_shot_produces_transmission() {
        let traffic = OneShot(Some(vec![0xAB]));
        let mut node = AlohaNode::new(NodeId(2), traffic, 100_000, 7);
        // Cycle until ReadyToTransmit.
        let mut time = 0u64;
        let tx = loop {
            let next = node.update(time);
            let t = node.poll_transmit(time);
            if t.is_some() {
                break t;
            }
            time = next.unwrap_or(time + 1_000);
        };
        assert!(tx.is_some());
        let tx = tx.unwrap();
        assert_eq!(tx.payload, vec![0xAB]);
        assert_eq!(tx.sf, 7);
        assert_eq!(tx.frequency, LORA_FREQUENCY);
        assert_eq!(tx.duration_us, LORA_SF7_DURATION_US);
    }

    #[test]
    fn poll_transmit_clears_payload() {
        let traffic = OneShot(Some(vec![0x01]));
        let mut node = AlohaNode::new(NodeId(3), traffic, 0, 5);
        let mut time = 0u64;
        let mut tx_count = 0;
        for _ in 0..200 {
            let next = node.update(time);
            if node.poll_transmit(time).is_some() {
                tx_count += 1;
            }
            time = next.unwrap_or(time + 1_000);
        }
        assert_eq!(tx_count, 1, "one-shot traffic should cause exactly one TX");
    }

    #[test]
    fn backoff_delays_next_tx() {
        // Two one-shots with a 500 ms backoff window.
        struct TwoShot(u8);
        impl TrafficModel for TwoShot {
            fn next_payload(&mut self, _time: SimTime) -> Option<Vec<u8>> {
                if self.0 > 0 {
                    self.0 -= 1;
                    Some(vec![self.0])
                } else {
                    None
                }
            }
        }

        let backoff_range = 500_000u64;
        let mut node = AlohaNode::new(NodeId(4), TwoShot(2), backoff_range, 11);
        let mut tx_times = Vec::new();
        let mut time = 0u64;
        for _ in 0..2_000_000 {
            let next = node.update(time);
            if node.poll_transmit(time).is_some() {
                tx_times.push(time);
            }
            if tx_times.len() == 2 {
                break;
            }
            time = next.unwrap_or(time + 1_000);
        }
        assert_eq!(tx_times.len(), 2, "should see exactly 2 transmissions");
        // After the first TX the node enters backoff (minimum = LORA_SF7_DURATION_US),
        // so the second TX must be at least one TX duration after the first.
        assert!(
            tx_times[1] >= tx_times[0] + LORA_SF7_DURATION_US,
            "second TX must be at least one TX-duration after the first; \
             first={} second={} duration={}",
            tx_times[0],
            tx_times[1],
            LORA_SF7_DURATION_US
        );
    }

    /// Calling `poll_transmit` while the node is in `Idle` phase must return
    /// `None` — the guard at the top of `poll_transmit` covers this path.
    #[test]
    fn poll_transmit_returns_none_when_not_ready() {
        let mut node = AlohaNode::new(NodeId(5), NeverFire, 0, 1);
        // Node starts in Idle — poll_transmit must return None.
        assert!(
            node.poll_transmit(0).is_none(),
            "poll_transmit must return None when not in ReadyToTransmit"
        );
    }

    /// Calling `update` a second time while already in `ReadyToTransmit`
    /// (without an intervening `poll_transmit`) must return `Some(time)` and
    /// leave the payload intact.
    #[test]
    fn update_returns_same_time_when_ready_to_transmit() {
        let traffic = OneShot(Some(vec![0xFF]));
        let mut node = AlohaNode::new(NodeId(6), traffic, 0, 2);
        // First update: traffic fires → phase becomes ReadyToTransmit.
        let t1 = node.update(0);
        assert_eq!(t1, Some(0), "should wake immediately when ready");
        // Second update (still in ReadyToTransmit, poll_transmit not yet called).
        let t2 = node.update(0);
        assert_eq!(
            t2,
            Some(0),
            "update while ReadyToTransmit must return the same time"
        );
        // Payload must still be available.
        assert!(
            node.poll_transmit(0).is_some(),
            "payload must survive the extra update call"
        );
    }

    /// Calling `update` with a time strictly before the backoff deadline must
    /// return `Some(until)` without advancing the phase — the "still waiting"
    /// path inside `Phase::Backoff`.
    #[test]
    fn backoff_still_waiting_returns_future_wake() {
        let traffic = OneShot(Some(vec![0x42]));
        // backoff_range_us = 100_000 → backoff ∈ [0, 100_000)
        let mut node = AlohaNode::new(NodeId(7), traffic, 100_000, 3);

        // Drive to ReadyToTransmit.
        node.update(0);
        // Transmit: phase → AwaitingBackoffStart.
        let tx = node.poll_transmit(0);
        assert!(tx.is_some());

        // update at t=0: phase → Backoff { until = 0 + 56_000 + backoff }.
        let until = node.update(0).expect("must return the backoff deadline");
        assert!(until >= LORA_SF7_DURATION_US, "deadline must be after on-air time");

        // Call update with time strictly before the deadline.
        let mid = until / 2;
        let again = node.update(mid);
        assert_eq!(
            again,
            Some(until),
            "still-waiting path must return the original deadline"
        );
    }

    /// `on_receive` is a no-op for Pure ALOHA; it must return `None` and not
    /// mutate the node state.
    #[test]
    fn on_receive_returns_none() {
        let mut node = AlohaNode::new(NodeId(8), NeverFire, 0, 1);
        let frame = RxMetadata {
            payload: vec![0xDE, 0xAD],
            rssi: -90.0,
            snr: 5.0,
            sf: 7,
            frequency: LORA_FREQUENCY,
            time: 0,
        };
        assert!(
            node.on_receive(frame, 0).is_none(),
            "Pure ALOHA on_receive must always return None"
        );
        // Node must still be idle after the receive event.
        assert!(
            node.poll_transmit(0).is_none(),
            "on_receive must not trigger a transmission"
        );
    }
}
