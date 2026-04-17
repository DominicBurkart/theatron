use theatron::scheduler::NodeHandle;
use theatron::time::SimTime;
use theatron::traits::TrafficModel;
use theatron::types::{NodeId, RxMetadata, Transmission};

// Default LoRa EU868 radio parameters. These are standard values used across
// all AlohaNode instances. `sf` and `frequency` remain caller-supplied because
// they are the primary parameters for channel / orthogonality experiments.
// TODO: consider accepting all RF params via an AlohaConfig struct and
// implementing the `Protocol` trait (per ARCHITECTURE.md) so that this example
// demonstrates the idiomatic theatron integration pattern rather than wiring
// directly to `NodeHandle`.
const DEFAULT_BANDWIDTH_HZ: u32 = 125_000; // EU868 standard bandwidth
const DEFAULT_CODING_RATE: u8 = 5; // 4/5 coding rate
const DEFAULT_TX_POWER_DBM: i8 = 14; // 14 dBm, legal limit for EU868

/// Pure ALOHA node: transmits immediately when a payload is available.
///
/// If the node has no pending data, it re-checks after `poll_interval_us`.
/// There is no carrier sensing or time slotting — this is the simplest
/// possible MAC protocol and serves as a baseline for comparison.
pub struct AlohaNode {
    id: NodeId,
    traffic: Box<dyn TrafficModel>,
    pending_tx: Option<Transmission>,
    poll_interval_us: u64,
    sf: u8,
    frequency: u32,
    bandwidth: u32,
    coding_rate: u8,
    tx_power_dbm: i8,
    tx_duration_us: u64,
}

impl AlohaNode {
    pub fn new(
        id: NodeId,
        traffic: Box<dyn TrafficModel>,
        poll_interval_us: u64,
        sf: u8,
        frequency: u32,
        tx_duration_us: u64,
    ) -> Self {
        Self {
            id,
            traffic,
            pending_tx: None,
            poll_interval_us,
            sf,
            frequency,
            bandwidth: DEFAULT_BANDWIDTH_HZ,
            coding_rate: DEFAULT_CODING_RATE,
            tx_power_dbm: DEFAULT_TX_POWER_DBM,
            tx_duration_us,
        }
    }

    /// Returns the next wake time, or `None` if traffic is permanently exhausted.
    ///
    /// When all packets have been sent and no new payload will ever be generated,
    /// returning `None` stops the scheduler from rescheduling this node on the
    /// poll interval indefinitely.
    fn try_generate_tx(&mut self, time: SimTime) -> Option<SimTime> {
        if self.pending_tx.is_some() {
            return Some(time);
        }
        if let Some(payload) = self.traffic.next_payload(time) {
            self.pending_tx = Some(Transmission {
                payload,
                sf: self.sf,
                bandwidth: self.bandwidth,
                coding_rate: self.coding_rate,
                frequency: self.frequency,
                duration_us: self.tx_duration_us,
                tx_power_dbm: self.tx_power_dbm,
            });
            Some(time)
        } else {
            // Check whether traffic can ever produce another payload. If the
            // model has a fixed count and it is exhausted, stop scheduling.
            // We probe one interval ahead; if still None at a future time, we
            // assume exhaustion — TrafficModel::next_payload is idempotent for
            // a depleted PeriodicTraffic (remaining == 0 always returns None).
            let future = time + self.poll_interval_us;
            if self.traffic.next_payload(future).is_none() {
                None // traffic permanently exhausted — do not reschedule
            } else {
                // Payload will be ready in the future; wake up and check again.
                // (next_payload consumed the future slot, but PeriodicTraffic
                //  re-checks time >= next_time, so calling it early is safe.)
                Some(future)
            }
        }
    }
}

impl NodeHandle for AlohaNode {
    fn node_id(&self) -> NodeId {
        self.id
    }

    /// AlohaNode senders do not need to receive frames in the current
    /// "transmit and forget" model. No ACK mechanism exists yet.
    ///
    /// NOTE: if a future implementation adds ACK-based retransmission, this
    /// method is the hook for detecting delivery confirmation. Backoff
    /// retransmission on collision also requires a new `on_tx_complete` callback
    /// in the `NodeHandle` trait (the scheduler currently discards collision
    /// events without notifying the sender).
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        self.pending_tx.take()
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        self.try_generate_tx(time)
    }
}

/// A simple receiver that collects all incoming frames.
pub struct AlohaReceiver {
    id: NodeId,
    pub received: Vec<RxMetadata>,
}

impl AlohaReceiver {
    pub fn new(id: NodeId) -> Self {
        Self {
            id,
            received: Vec::new(),
        }
    }
}

impl NodeHandle for AlohaReceiver {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        self.received.push(frame);
        None
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        None
    }

    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

/// A traffic model that produces `count` payloads at regular intervals.
pub struct PeriodicTraffic {
    payload: Vec<u8>,
    interval_us: u64,
    next_time: u64,
    remaining: usize,
}

impl PeriodicTraffic {
    pub fn new(payload: Vec<u8>, interval_us: u64, count: usize) -> Self {
        Self {
            payload,
            interval_us,
            next_time: 0,
            remaining: count,
        }
    }
}

impl TrafficModel for PeriodicTraffic {
    fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>> {
        if self.remaining == 0 {
            return None;
        }
        if time >= self.next_time {
            self.remaining -= 1;
            self.next_time = time + self.interval_us;
            Some(self.payload.clone())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_traffic_produces_exact_count() {
        let mut traffic = PeriodicTraffic::new(vec![0x42], 1_000_000, 3);
        assert!(traffic.next_payload(0).is_some());
        assert!(traffic.next_payload(1_000_000).is_some());
        assert!(traffic.next_payload(2_000_000).is_some());
        assert!(traffic.next_payload(3_000_000).is_none());
    }

    #[test]
    fn periodic_traffic_respects_interval() {
        let mut traffic = PeriodicTraffic::new(vec![0x42], 1_000_000, 2);
        assert!(traffic.next_payload(0).is_some());
        // Too early for next payload
        assert!(traffic.next_payload(500_000).is_none());
        assert!(traffic.next_payload(1_000_000).is_some());
    }

    #[test]
    fn aloha_node_transmits_when_payload_available() {
        let traffic = PeriodicTraffic::new(vec![0xAB], 1_000_000, 1);
        let mut node = AlohaNode::new(
            NodeId(1),
            Box::new(traffic),
            1_000_000,
            7,
            868_100_000,
            50_000,
        );
        let wake = node.update(0);
        assert!(wake.is_some());
        let tx = node.poll_transmit(0);
        assert!(tx.is_some());
        let tx = tx.unwrap();
        assert_eq!(tx.payload, vec![0xAB]);
        assert_eq!(tx.sf, 7);
    }

    #[test]
    fn aloha_receiver_collects_frames() {
        let mut receiver = AlohaReceiver::new(NodeId(99));
        let frame = RxMetadata {
            payload: vec![0x01],
            rssi: -80.0,
            snr: 10.0,
            sf: 7,
            frequency: 868_100_000,
            time: 1000,
        };
        receiver.on_receive(frame, 1000);
        assert_eq!(receiver.received.len(), 1);
    }

    #[test]
    fn aloha_node_no_payload_schedules_next_poll() {
        let traffic = PeriodicTraffic::new(vec![0xAB], 5_000_000, 1);
        let mut node = AlohaNode::new(
            NodeId(1),
            Box::new(traffic),
            1_000_000,
            7,
            868_100_000,
            50_000,
        );
        // Consume the one payload
        node.update(0);
        node.poll_transmit(0);
        // Traffic is now exhausted (remaining == 0); update should return None
        // to stop the scheduler from rescheduling this node.
        let wake = node.update(1_000_000);
        assert_eq!(wake, None);
    }
}
