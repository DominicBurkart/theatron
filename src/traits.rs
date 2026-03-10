use crate::time::SimTime;
use crate::types::{ChannelEvent, RxMetadata, Transmission};

pub trait Protocol {
    type Config;
    type State;
    type Metrics;

    fn init(&self, config: Self::Config) -> (Self::State, Option<SimTime>);
    fn on_receive(
        &self,
        state: &mut Self::State,
        frame: RxMetadata,
        time: SimTime,
    ) -> Option<SimTime>;
    fn poll_transmit(&self, state: &mut Self::State, time: SimTime) -> Option<Transmission>;
    fn update(&self, state: &mut Self::State, time: SimTime) -> Option<SimTime>;
    fn metrics(&self, state: &Self::State) -> Self::Metrics;
}

pub trait TrafficModel {
    fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>>;
}

pub trait InterferenceSource {
    fn observe(&mut self, event: &ChannelEvent, time: SimTime);
    fn poll_inject(&mut self, time: SimTime) -> Option<Transmission>;
    fn next_poll_time(&self, current_time: SimTime) -> Option<SimTime>;
}
