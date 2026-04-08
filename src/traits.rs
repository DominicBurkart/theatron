use crate::time::SimTime;
use crate::types::{ChannelEvent, RxMetadata, Transmission};

/// Defines how a node processes received frames and generates transmissions.
///
/// `init`, `on_receive`, and `update` each return `Option<SimTime>` — the next
/// simulation time at which the scheduler must call `update` on this node.
/// Returning `None` means no pending timer.
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

/// Provides uplink payloads to a node when it is ready to transmit.
///
/// Return `Some(payload)` to produce a frame, or `None` if no data is ready at `time`.
pub trait TrafficModel {
    fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>>;
}

/// Injects interference transmissions and observes channel events.
///
/// `observe` is called for every `ChannelEvent` (pre-collision-resolution),
/// matching real-world RF visibility. `poll_inject` returns a transmission to
/// inject, or `None`. `next_poll_time` controls when `poll_inject` is called again.
pub trait InterferenceSource {
    fn observe(&mut self, event: &ChannelEvent, time: SimTime);
    fn poll_inject(&mut self, time: SimTime) -> Option<Transmission>;
    fn next_poll_time(&self, current_time: SimTime) -> Option<SimTime>;
}
