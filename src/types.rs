use crate::time::SimTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone)]
pub struct RxMetadata {
    pub payload: Vec<u8>,
    pub rssi: f32,
    pub snr: f32,
    pub sf: u8,
    pub frequency: u32,
    pub time: SimTime,
}

#[derive(Debug, Clone)]
pub struct Transmission {
    pub payload: Vec<u8>,
    pub sf: u8,
    pub bandwidth: u32,
    pub coding_rate: u8,
    pub frequency: u32,
    pub duration_us: u64,
}

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
