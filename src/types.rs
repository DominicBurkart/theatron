use crate::time::SimTime;

/// A unique identifier for a simulation node.
///
/// # Examples
///
/// ```
/// use theatron::types::NodeId;
/// let id = NodeId(42);
/// assert_eq!(id, NodeId(42));
/// assert_ne!(id, NodeId(1));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

/// Metadata attached to a received radio frame.
///
/// # Examples
///
/// ```
/// use theatron::types::RxMetadata;
/// let meta = RxMetadata {
///     payload: vec![0x01, 0x02],
///     rssi: -80.0,
///     snr: 10.0,
///     sf: 7,
///     frequency: 868_100_000,
///     time: 50_000,
/// };
/// assert_eq!(meta.payload, vec![0x01, 0x02]);
/// assert_eq!(meta.sf, 7);
/// ```
#[derive(Debug, Clone)]
pub struct RxMetadata {
    pub payload: Vec<u8>,
    pub rssi: f32,
    pub snr: f32,
    pub sf: u8,
    pub frequency: u32,
    pub time: SimTime,
}

/// Parameters for a radio transmission.
///
/// # Examples
///
/// ```
/// use theatron::types::Transmission;
/// let tx = Transmission {
///     payload: vec![0xDE, 0xAD],
///     sf: 7,
///     bandwidth: 125_000,
///     coding_rate: 5,
///     frequency: 868_100_000,
///     duration_us: 50_000,
///     tx_power_dbm: 14,
/// };
/// assert_eq!(tx.payload.len(), 2);
/// assert_eq!(tx.sf, 7);
/// ```
#[derive(Debug, Clone)]
pub struct Transmission {
    pub payload: Vec<u8>,
    pub sf: u8,
    pub bandwidth: u32,
    pub coding_rate: u8,
    pub frequency: u32,
    pub duration_us: u64,
    pub tx_power_dbm: i8,
}

/// Events emitted by the channel during a simulation.
///
/// # Examples
///
/// ```
/// use theatron::types::{ChannelEvent, NodeId};
/// let started = ChannelEvent::TransmissionStarted {
///     sender: NodeId(1),
///     sf: 7,
///     frequency: 868_100_000,
///     time: 0,
/// };
/// match started {
///     ChannelEvent::TransmissionStarted { sender, .. } => assert_eq!(sender, NodeId(1)),
///     _ => panic!("expected TransmissionStarted"),
/// }
/// ```
#[derive(Debug, Clone)]
pub enum ChannelEvent {
    TransmissionStarted {
        sender: NodeId,
        sf: u8,
        frequency: u32,
        time: SimTime,
    },
    TransmissionCompleted {
        sender: NodeId,
        time: SimTime,
        collided: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn node_id_in_hashset() {
        let mut set = HashSet::new();
        set.insert(NodeId(1));
        set.insert(NodeId(2));
        set.insert(NodeId(1));
        assert_eq!(set.len(), 2);
        assert!(set.contains(&NodeId(1)));
    }

    #[test]
    fn rx_metadata_clone_independence() {
        let meta = RxMetadata {
            payload: vec![0x01],
            rssi: -80.0,
            snr: 10.0,
            sf: 7,
            frequency: 868_100_000,
            time: 0,
        };
        let mut cloned = meta.clone();
        cloned.payload.push(0x02);
        assert_eq!(meta.payload.len(), 1);
        assert_eq!(cloned.payload.len(), 2);
    }

    #[test]
    fn transmission_clone_independence() {
        let tx = Transmission {
            payload: vec![0x01],
            sf: 7,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency: 868_100_000,
            duration_us: 50_000,
            tx_power_dbm: 14,
        };
        let mut cloned = tx.clone();
        cloned.payload.push(0x02);
        assert_eq!(tx.payload.len(), 1);
        assert_eq!(cloned.payload.len(), 2);
    }
}
