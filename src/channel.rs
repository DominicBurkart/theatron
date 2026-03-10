use crate::time::SimTime;
use crate::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

struct ActiveTransmission {
    sender: NodeId,
    payload: Vec<u8>,
    sf: u8,
    #[allow(dead_code)]
    bandwidth: u32,
    frequency: u32,
    start: SimTime,
    end: SimTime,
    collided: bool,
}

/// A simulated wireless channel with collision detection.
pub struct Channel {
    active: Vec<ActiveTransmission>,
    completed: Vec<ActiveTransmission>,
}

impl Channel {
    /// Create a new empty channel.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::Channel;
    /// let ch = Channel::new();
    /// ```
    pub fn new() -> Self {
        Self {
            active: Vec::new(),
            completed: Vec::new(),
        }
    }

    /// Begin a transmission on the channel, returning a `TransmissionStarted` event.
    ///
    /// Collisions are detected immediately: if the new transmission overlaps in time with
    /// an existing active transmission on the same SF and frequency, both are marked collided.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::Channel;
    /// use theatron::types::{NodeId, Transmission};
    ///
    /// let mut ch = Channel::new();
    /// let tx = Transmission {
    ///     payload: vec![0x01],
    ///     sf: 7,
    ///     bandwidth: 125_000,
    ///     coding_rate: 5,
    ///     frequency: 868_100_000,
    ///     duration_us: 50_000,
    /// };
    /// let event = ch.begin_transmission(NodeId(1), &tx, 0);
    /// ```
    pub fn begin_transmission(
        &mut self,
        sender: NodeId,
        tx: &Transmission,
        time: SimTime,
    ) -> ChannelEvent {
        let end = time + tx.duration_us;
        let mut new_collided = false;
        for active in &mut self.active {
            if overlaps(active.start, active.end, time, end)
                && active.frequency == tx.frequency
                && active.sf == tx.sf
            {
                active.collided = true;
                new_collided = true;
            }
        }
        self.active.push(ActiveTransmission {
            sender,
            payload: tx.payload.clone(),
            sf: tx.sf,
            bandwidth: tx.bandwidth,
            frequency: tx.frequency,
            start: time,
            end,
            collided: new_collided,
        });
        ChannelEvent::TransmissionStarted {
            sender,
            sf: tx.sf,
            frequency: tx.frequency,
            time,
        }
    }

    /// Move all active transmissions that ended at or before `time` to the completed list,
    /// returning a `TransmissionCompleted` event for each.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::Channel;
    /// use theatron::types::{NodeId, Transmission};
    ///
    /// let mut ch = Channel::new();
    /// let tx = Transmission {
    ///     payload: vec![0x01],
    ///     sf: 7,
    ///     bandwidth: 125_000,
    ///     coding_rate: 5,
    ///     frequency: 868_100_000,
    ///     duration_us: 50_000,
    /// };
    /// ch.begin_transmission(NodeId(1), &tx, 0);
    /// let events = ch.resolve_at(50_000);
    /// assert_eq!(events.len(), 1);
    /// ```
    pub fn resolve_at(&mut self, time: SimTime) -> Vec<ChannelEvent> {
        let mut events = Vec::new();
        let mut remaining = Vec::new();
        for tx in self.active.drain(..) {
            if tx.end <= time {
                events.push(ChannelEvent::TransmissionCompleted {
                    sender: tx.sender,
                    time: tx.end,
                    collided: tx.collided,
                });
                self.completed.push(tx);
            } else {
                remaining.push(tx);
            }
        }
        self.active = remaining;
        events
    }

    /// Return all completed, non-collided transmissions as received frames.
    ///
    /// The `_node_id` parameter is accepted for API symmetry but does not filter results.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::Channel;
    /// use theatron::types::{NodeId, Transmission};
    ///
    /// let mut ch = Channel::new();
    /// let tx = Transmission {
    ///     payload: vec![0x01],
    ///     sf: 7,
    ///     bandwidth: 125_000,
    ///     coding_rate: 5,
    ///     frequency: 868_100_000,
    ///     duration_us: 50_000,
    /// };
    /// ch.begin_transmission(NodeId(1), &tx, 0);
    /// ch.resolve_at(50_000);
    /// let received = ch.deliver_to(NodeId(2), 50_000);
    /// assert_eq!(received.len(), 1);
    /// assert_eq!(received[0].payload, vec![0x01]);
    /// ```
    pub fn deliver_to(&self, _node_id: NodeId, time: SimTime) -> Vec<RxMetadata> {
        self.completed
            .iter()
            .filter(|tx| tx.end <= time && !tx.collided)
            .map(|tx| RxMetadata {
                payload: tx.payload.clone(),
                rssi: -80.0,
                snr: 10.0,
                sf: tx.sf,
                frequency: tx.frequency,
                time: tx.end,
            })
            .collect()
    }

    /// Drain and return all completed transmissions as `(sender, collided, payload, sf, frequency, end_time)` tuples.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::Channel;
    /// use theatron::types::{NodeId, Transmission};
    ///
    /// let mut ch = Channel::new();
    /// let tx = Transmission {
    ///     payload: vec![0x01],
    ///     sf: 7,
    ///     bandwidth: 125_000,
    ///     coding_rate: 5,
    ///     frequency: 868_100_000,
    ///     duration_us: 50_000,
    /// };
    /// ch.begin_transmission(NodeId(1), &tx, 0);
    /// ch.resolve_at(50_000);
    /// let completed = ch.drain_completed();
    /// assert_eq!(completed.len(), 1);
    /// assert_eq!(completed[0].0, NodeId(1));
    /// assert!(!completed[0].1);
    /// ```
    pub fn drain_completed(&mut self) -> Vec<(NodeId, bool, Vec<u8>, u8, u32, SimTime)> {
        self.completed
            .drain(..)
            .map(|tx| {
                (
                    tx.sender,
                    tx.collided,
                    tx.payload,
                    tx.sf,
                    tx.frequency,
                    tx.end,
                )
            })
            .collect()
    }
}

impl Default for Channel {
    fn default() -> Self {
        Self::new()
    }
}

fn overlaps(a_start: SimTime, a_end: SimTime, b_start: SimTime, b_end: SimTime) -> bool {
    a_start < b_end && b_start < a_end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Transmission;
    use proptest::prelude::*;

    fn make_tx(sf: u8, frequency: u32, duration_us: u64) -> Transmission {
        Transmission {
            payload: vec![0x01, 0x02],
            sf,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency,
            duration_us,
        }
    }

    #[test]
    fn single_transmission_delivers() {
        let mut ch = Channel::new();
        let tx = make_tx(7, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx, 0);
        ch.resolve_at(50_000);
        let delivered = ch.deliver_to(NodeId(2), 50_000);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].payload, vec![0x01, 0x02]);
    }

    #[test]
    fn same_sf_same_freq_overlap_collides() {
        let mut ch = Channel::new();
        let tx1 = make_tx(7, 868_100_000, 50_000);
        let tx2 = make_tx(7, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.begin_transmission(NodeId(2), &tx2, 10_000);
        ch.resolve_at(60_000);
        let delivered = ch.deliver_to(NodeId(3), 60_000);
        assert_eq!(delivered.len(), 0);
    }

    #[test]
    fn different_sf_no_collision() {
        let mut ch = Channel::new();
        let tx1 = make_tx(7, 868_100_000, 50_000);
        let tx2 = make_tx(8, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.begin_transmission(NodeId(2), &tx2, 10_000);
        ch.resolve_at(60_000);
        let delivered = ch.deliver_to(NodeId(3), 60_000);
        assert_eq!(delivered.len(), 2);
    }

    #[test]
    fn different_frequency_no_collision() {
        let mut ch = Channel::new();
        let tx1 = make_tx(7, 868_100_000, 50_000);
        let tx2 = make_tx(7, 868_300_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.begin_transmission(NodeId(2), &tx2, 10_000);
        ch.resolve_at(60_000);
        let delivered = ch.deliver_to(NodeId(3), 60_000);
        assert_eq!(delivered.len(), 2);
    }

    #[test]
    fn non_overlapping_transmissions_no_collision() {
        let mut ch = Channel::new();
        let tx1 = make_tx(7, 868_100_000, 50_000);
        let tx2 = make_tx(7, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.resolve_at(50_000);
        ch.drain_completed();
        ch.begin_transmission(NodeId(2), &tx2, 60_000);
        ch.resolve_at(110_000);
        let delivered = ch.deliver_to(NodeId(3), 110_000);
        assert_eq!(delivered.len(), 1);
    }

    #[test]
    fn exactly_adjacent_no_collision() {
        let mut ch = Channel::new();
        let tx1 = make_tx(7, 868_100_000, 50_000);
        let tx2 = make_tx(7, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.resolve_at(50_000);
        ch.drain_completed();
        ch.begin_transmission(NodeId(2), &tx2, 50_000);
        ch.resolve_at(100_000);
        let completed = ch.drain_completed();
        assert!(completed.iter().all(|(_, collided, _, _, _, _)| !collided));
    }

    #[test]
    fn three_way_collision_marks_all() {
        let mut ch = Channel::new();
        for i in 1u32..=3 {
            let tx = make_tx(7, 868_100_000, 50_000);
            ch.begin_transmission(NodeId(i), &tx, 10_000 * (i as u64 - 1));
        }
        ch.resolve_at(70_000);
        let completed = ch.drain_completed();
        assert_eq!(completed.len(), 3);
        assert!(completed.iter().all(|(_, collided, _, _, _, _)| *collided));
    }

    proptest! {
        #[test]
        fn n_non_overlapping_txs_never_collide(n in 2usize..20) {
            let mut ch = Channel::new();
            let duration = 50_000u64;
            let gap = 10_000u64;
            let mut all_clean = true;
            let mut t = 0u64;
            for _ in 0..n {
                let tx = make_tx(7, 868_100_000, duration);
                ch.begin_transmission(NodeId(1), &tx, t);
                ch.resolve_at(t + duration);
                let completed = ch.drain_completed();
                if completed.iter().any(|(_, collided, _, _, _, _)| *collided) {
                    all_clean = false;
                    break;
                }
                t += duration + gap;
            }
            prop_assert!(all_clean);
        }

        #[test]
        fn n_simultaneous_same_sf_freq_all_collide(n in 2usize..10) {
            let mut ch = Channel::new();
            let duration = 50_000u64;
            for i in 0..n {
                let tx = make_tx(7, 868_100_000, duration);
                ch.begin_transmission(NodeId(i as u32), &tx, 0);
            }
            ch.resolve_at(duration);
            let completed = ch.drain_completed();
            prop_assert_eq!(completed.len(), n);
            prop_assert!(completed.iter().all(|(_, collided, _, _, _, _)| *collided));
        }

        #[test]
        fn different_sf_never_collide(sf_a in 7u8..13u8, sf_b in 7u8..13u8) {
            prop_assume!(sf_a != sf_b);
            let mut ch = Channel::new();
            let duration = 50_000u64;
            let tx_a = make_tx(sf_a, 868_100_000, duration);
            let tx_b = make_tx(sf_b, 868_100_000, duration);
            ch.begin_transmission(NodeId(1), &tx_a, 0);
            ch.begin_transmission(NodeId(2), &tx_b, 0);
            ch.resolve_at(duration);
            let completed = ch.drain_completed();
            prop_assert!(completed.iter().all(|(_, collided, _, _, _, _)| !collided));
        }
    }
}
