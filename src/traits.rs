use crate::time::SimTime;
use crate::types::{ChannelEvent, RxMetadata, Transmission};

/// A protocol defines how a node processes received frames and generates transmissions.
///
/// # Examples
///
/// ```
/// use theatron::traits::Protocol;
/// use theatron::types::{RxMetadata, Transmission};
///
/// struct NoOp;
///
/// impl Protocol for NoOp {
///     type Config = ();
///     type State = ();
///     type Metrics = ();
///
///     fn init(&self, _config: ()) -> ((), Option<u64>) { ((), None) }
///     fn on_receive(&self, _state: &mut (), _frame: RxMetadata, _time: u64) -> Option<u64> { None }
///     fn poll_transmit(&self, _state: &mut (), _time: u64) -> Option<Transmission> { None }
///     fn update(&self, _state: &mut (), _time: u64) -> Option<u64> { None }
///     fn metrics(&self, _state: &()) {}
/// }
///
/// let p = NoOp;
/// let (_, wake) = p.init(());
/// assert!(wake.is_none());
/// ```
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

/// A traffic model determines what payloads a node generates and when.
///
/// # Examples
///
/// ```
/// use theatron::traits::TrafficModel;
///
/// struct FixedPayload(Option<Vec<u8>>);
///
/// impl TrafficModel for FixedPayload {
///     fn next_payload(&mut self, _time: u64) -> Option<Vec<u8>> {
///         self.0.take()
///     }
/// }
///
/// let mut model = FixedPayload(Some(vec![0x01, 0x02]));
/// assert_eq!(model.next_payload(0), Some(vec![0x01, 0x02]));
/// assert_eq!(model.next_payload(1), None);
/// ```
pub trait TrafficModel {
    fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>>;
}

/// An interference source can inject transmissions and observe channel events.
///
/// # Examples
///
/// ```
/// use theatron::traits::InterferenceSource;
/// use theatron::types::{ChannelEvent, Transmission};
///
/// struct NullInterferer;
///
/// impl InterferenceSource for NullInterferer {
///     fn observe(&mut self, _event: &ChannelEvent, _time: u64) {}
///     fn poll_inject(&mut self, _time: u64) -> Option<Transmission> { None }
///     fn next_poll_time(&self, _current_time: u64) -> Option<u64> { None }
/// }
///
/// let mut ni = NullInterferer;
/// assert!(ni.poll_inject(0).is_none());
/// assert!(ni.next_poll_time(0).is_none());
/// ```
pub trait InterferenceSource {
    fn observe(&mut self, event: &ChannelEvent, time: SimTime);
    fn poll_inject(&mut self, time: SimTime) -> Option<Transmission>;
    fn next_poll_time(&self, current_time: SimTime) -> Option<SimTime>;
}
