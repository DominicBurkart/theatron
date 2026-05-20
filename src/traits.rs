use crate::time::SimTime;
use crate::types::{ChannelEvent, RxMetadata, Transmission};

/// A protocol defines how a node processes received frames and generates transmissions.
///
/// # Examples
///
/// ```
/// use theatron::time::SimTime;
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
///     fn init(&self, _config: ()) -> ((), Option<SimTime>) { ((), None) }
///     fn on_receive(&self, _state: &mut (), _frame: RxMetadata, _time: SimTime) -> Option<SimTime> { None }
///     fn poll_transmit(&self, _state: &mut (), _time: SimTime) -> Option<Transmission> { None }
///     fn update(&self, _state: &mut (), _time: SimTime) -> Option<SimTime> { None }
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
/// use theatron::time::SimTime;
/// use theatron::traits::TrafficModel;
///
/// struct FixedPayload(Option<Vec<u8>>);
///
/// impl TrafficModel for FixedPayload {
///     fn next_payload(&mut self, _time: SimTime) -> Option<Vec<u8>> {
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
/// use theatron::time::SimTime;
/// use theatron::traits::InterferenceSource;
/// use theatron::types::{ChannelEvent, Transmission};
///
/// struct NullInterferer;
///
/// impl InterferenceSource for NullInterferer {
///     fn observe(&mut self, _event: &ChannelEvent, _time: SimTime) {}
///     fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> { None }
///     fn next_poll_time(&self, _current_time: SimTime) -> Option<SimTime> { None }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RxMetadata, Transmission};

    /// A minimal `Protocol` implementor that does nothing.
    ///
    /// This exists to:
    /// 1. Verify that the `Protocol` trait can be implemented with unit
    ///    associated types — acting as a compile-time contract test.
    /// 2. Document every required method and its expected return type.
    ///
    /// Note: `Protocol` is separate from `NodeHandle`, which is what the
    /// `Scheduler` actually drives.  A future integration layer would wrap
    /// a `Protocol` + per-node state into a `NodeHandle`.  Until that
    /// adapter exists, the tests below exercise the trait surface directly.
    struct NoOpProtocol;

    impl Protocol for NoOpProtocol {
        type Config = ();
        type State = ();
        type Metrics = ();

        fn init(&self, _config: ()) -> ((), Option<SimTime>) {
            ((), None)
        }

        fn on_receive(
            &self,
            _state: &mut (),
            _frame: RxMetadata,
            _time: SimTime,
        ) -> Option<SimTime> {
            None
        }

        fn poll_transmit(&self, _state: &mut (), _time: SimTime) -> Option<Transmission> {
            None
        }

        fn update(&self, _state: &mut (), _time: SimTime) -> Option<SimTime> {
            None
        }

        fn metrics(&self, _state: &()) {}
    }

    #[test]
    fn no_op_protocol_init_returns_no_wake() {
        let p = NoOpProtocol;
        let (state, wake) = p.init(());
        assert!(wake.is_none(), "NoOpProtocol should not schedule an initial wake");
        // Bind `state` so rustc confirms the associated type resolves to `()`.
        let _: () = state;
    }

    #[test]
    fn no_op_protocol_on_receive_returns_none() {
        let p = NoOpProtocol;
        let mut state = ();
        let frame = RxMetadata {
            payload: vec![0xAB],
            rssi: -80.0,
            snr: 10.0,
            sf: 7,
            frequency: 868_100_000,
            time: 0,
        };
        let next_wake = p.on_receive(&mut state, frame, 0);
        assert!(next_wake.is_none());
    }

    #[test]
    fn no_op_protocol_poll_transmit_returns_none() {
        let p = NoOpProtocol;
        let mut state = ();
        let tx = p.poll_transmit(&mut state, 0);
        assert!(tx.is_none());
    }

    #[test]
    fn no_op_protocol_update_returns_none() {
        let p = NoOpProtocol;
        let mut state = ();
        let next_wake = p.update(&mut state, 0);
        assert!(next_wake.is_none());
    }

    #[test]
    fn no_op_protocol_metrics_compiles_and_returns_unit() {
        let p = NoOpProtocol;
        let state = ();
        // `metrics` returns `Self::Metrics` which is `()` for NoOpProtocol.
        // This test ensures the method is callable and the return type resolves.
        let result: () = p.metrics(&state);
        let _ = result;
    }
}
