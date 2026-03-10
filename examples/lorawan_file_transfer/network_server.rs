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
