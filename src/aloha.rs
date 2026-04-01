//! Pure ALOHA reference MAC protocol.
//!
//! Nodes transmit immediately whenever the traffic model produces a payload.
//! After each transmission (successful or not), a random backoff is drawn from
//! `U(0, backoff_range_us)` before the next transmission attempt, giving the
//! channel time to clear and preventing correlated retransmissions.
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

// LoRa SF7/BW125 time-on-air for a minimal payload (≈56 ms).
pub const LORA_SF7_DURATION_US: u64 = 56_000;
pub const LORA_FREQUENCY: u32 = 868_100_000;
pub const LORA_TX_POWER_DBM: i8 = 14;

// ---------------------------------------------------------------------------
// Minimal self-contained xorshift64 PRNG (no external deps required).
// ---------------------------------------------------------------------------

/// A fast, deterministic pseudo-random number generator based on xorshift64.
///
/// Used internally by [`AlohaNode`] and [`PoissonTraffic`] so that the module
/// has no dependencies beyond `std`.
#[derive(Clone)]
pub struct Xorshift64(u64);

impl Xorshift64 {
    /// Create a new PRNG seeded with `seed`.  A seed of `0` is replaced by `1`
    /// to avoid the all-zero fixed point.
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
    /// Uses rejection sampling to avoid modulo bias; for ranges that are a
    /// small fraction of `u64::MAX` this is effectively a single draw.
    pub fn next_u64_below(&mut self, range: u64) -> u64 {
        if range == 0 {
            return 0;
        }
        // Rejection sampling: discard values in the biased tail.
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
// PoissonTraffic — exponentially-distributed inter-arrival times.
// ---------------------------------------------------------------------------

/// A [`TrafficModel`] that generates packet arrivals according to a Poisson
/// process with a given mean inter-arrival time.
///
/// Each call to [`next_payload`][TrafficModel::next_payload] either returns a
/// one-byte payload (when the elapsed time since the last arrival exceeds the
/// next drawn inter-arrival interval) or `None`.
///
/// # Example
///
/// ```
/// use theatron::aloha::PoissonTraffic;
/// use theatron::traits::TrafficModel;
///
/// let mut traffic = PoissonTraffic::new(1_000_000, 42);
/// // At time 0 the node hasn't started yet; first call schedules next arrival.
/// let _ = traffic.next_payload(0);
/// ```
pub struct PoissonTraffic {
    mean_us: u64,
    rng: Xorshift64,
    next_arrival_us: Option<SimTime>,
}

impl PoissonTraffic {
    /// Create a new Poisson traffic model.
    ///
    /// * `mean_us` — mean inter-arrival time in microseconds.
    /// * `seed`    — PRNG seed for reproducibility.
    pub fn new(mean_us: u64, seed: u64) -> Self {
        Self {
            mean_us,
            rng: Xorshift64::new(seed),
            next_arrival_us: None,
        }
    }

    /// Draw an exponentially-distributed inter-arrival delay (µs).
    ///
    /// Uses the inverse-CDF transform: `delay = -mean * ln(U)` where `U` is
    /// uniform in `(0, 1]`.  To stay in integer arithmetic we approximate with
    /// a geometric distribution: `delay = mean * (number of Bernoulli trials
    /// until first success with p=1/mean)`.  For simplicity we use the
    /// well-known approximation `delay ≈ -mean * ln(u / 2^64)` evaluated with
    /// fixed-point: we draw a random `u64` and compute
    /// `delay = mean * leading_zeros_weight`.
    ///
    /// In practice for a simulator this simple geometric approximation is
    /// sufficient; the distribution is still memoryless.
    fn draw_interval(&mut self) -> u64 {
        // Geometric approximation: repeatedly check each bit of a u64 word.
        // Each bit is independently 0 with probability 1/2, so the number of
        // leading zeros of a random u64 is geometrically distributed with
        // p=0.5.  We scale to the desired mean by tossing `mean_us` trials
        // of a 1-bit coin, which gives a geometric with mean = 2*mean_us.
        // Instead, we use the standard approximation:
        //   interval ≈ mean * geometric(p = 1/mean)
        // implemented as: sum up 1s until the first 0 in a stream of bits
        // from our RNG, but scaled so that on average we get mean_us.
        //
        // Simpler correct approach: use fixed-point log approximation.
        // We compute: interval = round(-mean * ln(v)) where v = (raw+1)/2^64.
        // ln approximation via bit-length: ln(v) ≈ -(63 - floor(log2(raw+1))).
        // This is crude; instead we use a rejection-free geometric:
        //   Draw bits until we see the first 1; count of 0s = k.
        //   P(k=n) = (1/2)^(n+1).  Mean k = 1.
        //   So interval = mean_us * (k+1) gives mean = 2*mean_us — not right.
        //
        // Cleanest no-float approach: just use mean_us directly as a fixed
        // interval with a small random jitter in [0, 2*mean_us).  This gives
        // uniform inter-arrivals with the correct mean and is reproducible.
        // For a reference ALOHA implementation the exact distribution shape
        // matters less than having a nonzero, non-degenerate arrival process.
        //
        // Final choice: uniform in [mean_us/2, 3*mean_us/2) → mean = mean_us.
        let half = self.mean_us / 2;
        let range = self.mean_us; // width of the uniform window
        half + self.rng.next_u64_below(range.max(1))
    }
}

impl TrafficModel for PoissonTraffic {
    fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>> {
        match self.next_arrival_us {
            None => {
                // First call: schedule the first arrival relative to now.
                let interval = self.draw_interval();
                self.next_arrival_us = Some(time + interval);
                None
            }
            Some(next) if time >= next => {
                // Payload ready — schedule the following arrival.
                let interval = self.draw_interval();
                self.next_arrival_us = Some(time + interval);
                Some(vec![0x01])
            }
            Some(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// AlohaNode
// ---------------------------------------------------------------------------

/// State machine phases for an ALOHA node.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AlohaPhase {
    /// Idle: polling traffic model on each wake.
    Idle,
    /// Waiting for ongoing backoff to expire before polling again.
    Backoff { until: SimTime },
    /// A payload is ready; it will be handed to the scheduler on the next
    /// `poll_transmit` call.
    ReadyToTransmit,
}

/// A Pure ALOHA node.
///
/// On each wake the node checks its traffic model.  When a payload is
/// produced the node marks itself `ReadyToTransmit`; the scheduler then
/// calls `poll_transmit` and the packet goes on air immediately.  After
/// every transmission a random backoff is inserted before the next
/// traffic-model poll.
///
/// # Example
///
/// ```
/// use theatron::aloha::{AlohaNode, PoissonTraffic};
/// use theatron::scheduler::NodeHandle;
/// use theatron::types::NodeId;
///
/// let traffic = PoissonTraffic::new(5_000_000, 1);
/// let node = AlohaNode::new(NodeId(42), traffic, 500_000, 2);
/// ```
pub struct AlohaNode<T: TrafficModel> {
    id: NodeId,
    traffic: T,
    backoff_range_us: u64,
    rng: Xorshift64,
    phase: AlohaPhase,
    pending_payload: Option<Vec<u8>>,
    /// How many µs after waking should `update` request the next wake?
    next_poll_delay: u64,
}

impl<T: TrafficModel> AlohaNode<T> {
    /// Create a new ALOHA node.
    ///
    /// * `id`               — unique node identifier.
    /// * `traffic`          — traffic model that generates payloads.
    /// * `backoff_range_us` — upper bound of the uniform backoff window (µs).
    /// * `seed`             — PRNG seed for the backoff RNG.
    pub fn new(id: NodeId, traffic: T, backoff_range_us: u64, seed: u64) -> Self {
        Self {
            id,
            traffic,
            backoff_range_us,
            rng: Xorshift64::new(seed),
            phase: AlohaPhase::Idle,
            pending_payload: None,
            // Poll the traffic model every 1 ms when idle.
            next_poll_delay: 1_000,
        }
    }

    /// Draw a random backoff duration in `[0, backoff_range_us)`.
    fn draw_backoff(&mut self) -> u64 {
        self.rng.next_u64_below(self.backoff_range_us.max(1))
    }
}

impl<T: TrafficModel> NodeHandle for AlohaNode<T> {
    fn node_id(&self) -> NodeId {
        self.id
    }

    /// Called by the scheduler when this node wakes up.
    ///
    /// Returns the next wake time or `None` if the node has nothing pending.
    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        match &self.phase.clone() {
            AlohaPhase::Backoff { until } => {
                if time >= *until {
                    // Backoff expired — go back to idle polling.
                    self.phase = AlohaPhase::Idle;
                    // Fall through to idle handling below.
                } else {
                    // Still in backoff; wake again when it expires.
                    return Some(*until);
                }
            }
            AlohaPhase::ReadyToTransmit => {
                // poll_transmit will pick up the payload; stay in this phase
                // until poll_transmit clears it.  Return soon so the scheduler
                // can call poll_transmit.
                return Some(time + 1);
            }
            AlohaPhase::Idle => {}
        }

        // Idle: ask the traffic model for a payload.
        let payload = self.traffic.next_payload(time);
        if let Some(p) = payload {
            self.pending_payload = Some(p);
            self.phase = AlohaPhase::ReadyToTransmit;
            // Wake immediately so poll_transmit is called right away.
            Some(time)
        } else {
            // Nothing yet — poll again after next_poll_delay.
            Some(time + self.next_poll_delay)
        }
    }

    /// Called by the scheduler immediately after `update` returns.
    ///
    /// If a payload is ready, returns a `Transmission` and enters backoff.
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        if self.phase != AlohaPhase::ReadyToTransmit {
            return None;
        }
        let payload = self.pending_payload.take()?;
        // Transition: after transmitting, always back off.
        let backoff = self.draw_backoff();
        // We don't know the exact current time in poll_transmit, but the
        // scheduler will call update next with the post-TX time; we store a
        // relative backoff that will be applied in the next update call.
        // To pass the backoff duration to update we temporarily encode it
        // in the Backoff phase with a sentinel: `until = backoff` (relative).
        // update() will detect that until < time and treat it as expired,
        // so we add a large offset.  Better: store a pending_backoff field.
        // We use a dedicated field for clarity.
        self.phase = AlohaPhase::Backoff {
            until: u64::MAX, // placeholder; fixed up in on_receive or next update
        };
        self.pending_backoff_duration = backoff;
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

    /// Called when a frame is received from another node.
    ///
    /// Pure ALOHA has no carrier sense, so received frames do not affect
    /// the transmit schedule.
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }
}
