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

pub struct Channel {
    active: Vec<ActiveTransmission>,
    completed: Vec<ActiveTransmission>,
}

impl Channel {
    pub fn new() -> Self {
        Self {
            active: Vec::new(),
            completed: Vec::new(),
        }
    }

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
}
