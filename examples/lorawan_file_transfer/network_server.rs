use theatron::scheduler::NodeHandle;
use theatron::time::SimTime;
use theatron::types::{NodeId, RxMetadata, Transmission};

pub struct NetworkServer {
    id: NodeId,
    received_fragments: Vec<Vec<u8>>,
    total_bytes_received: usize,
}

impl NetworkServer {
    pub fn new(id: NodeId) -> Self {
        Self {
            id,
            received_fragments: Vec::new(),
            total_bytes_received: 0,
        }
    }

    #[allow(dead_code)]
    pub fn received_fragments(&self) -> &[Vec<u8>] {
        &self.received_fragments
    }

    #[allow(dead_code)]
    pub fn total_bytes_received(&self) -> usize {
        self.total_bytes_received
    }
}

impl NodeHandle for NetworkServer {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        self.total_bytes_received += frame.payload.len();
        self.received_fragments.push(frame.payload);
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
    fn new_server_empty() {
        let server = NetworkServer::new(NodeId(100));
        assert_eq!(server.received_fragments().len(), 0);
        assert_eq!(server.total_bytes_received(), 0);
    }

    #[test]
    fn on_receive_accumulates() {
        let mut server = NetworkServer::new(NodeId(100));
        server.on_receive(make_frame(vec![0x01, 0x02]), 0);
        server.on_receive(make_frame(vec![0x03]), 1000);
        assert_eq!(server.received_fragments().len(), 2);
        assert_eq!(server.total_bytes_received(), 3);
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
}
