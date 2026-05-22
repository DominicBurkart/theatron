use theatron::scheduler::NodeHandle;
use theatron::time::SimTime;
use theatron::types::{NodeId, RxMetadata, Transmission};

/// Stub LoRaWAN network server used by the file-transfer example.
///
/// The architecture treats the network server as an external simulation
/// participant; for this example we only need a node that consumes uplinks
/// so the scheduler-level RX metrics (`MetricsCollector::node_rx_count`)
/// reflect end-to-end delivery. Per-fragment payload accumulation was
/// previously stored on this struct but never read, so the receive path is
/// now a no-op and the bookkeeping fields are gone.
pub struct NetworkServer {
    id: NodeId,
}

impl NetworkServer {
    pub fn new(id: NodeId) -> Self {
        Self { id }
    }
}

impl NodeHandle for NetworkServer {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(payload: Vec<u8>) -> RxMetadata {
        RxMetadata {
            payload,
            rssi: -80.0,
            snr: 10.0,
            sf: 7,
            frequency: 868_100_000,
            time: 0,
        }
    }

    #[test]
    fn on_receive_returns_none() {
        let mut server = NetworkServer::new(NodeId(100));
        assert!(server.on_receive(make_frame(vec![0x01, 0x02]), 0).is_none());
        assert!(server.on_receive(make_frame(vec![0x03]), 1000).is_none());
    }

    #[test]
    fn poll_transmit_always_none() {
        let mut server = NetworkServer::new(NodeId(100));
        assert!(server.poll_transmit(0).is_none());
        assert!(server.poll_transmit(1_000_000).is_none());
    }

    #[test]
    fn update_always_none() {
        let mut server = NetworkServer::new(NodeId(100));
        assert!(server.update(0).is_none());
        assert!(server.update(1_000_000).is_none());
    }

    #[test]
    fn node_id_returned_unchanged() {
        let server = NetworkServer::new(NodeId(100));
        assert_eq!(server.node_id(), NodeId(100));
    }
}
