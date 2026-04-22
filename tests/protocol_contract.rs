//! Contract tests for the Protocol trait.
//!
//! These tests verify that a conforming implementation satisfies the
//! behavioural expectations of the trait: init returns a wake time,
//! poll_transmit drains after the first call, and on_receive is safe
//! to call at any time.

use theatron::traits::Protocol;
use theatron::types::{RxMetadata, Transmission};

/// The simplest possible Protocol implementation: transmits one fixed
/// frame on the first poll, then goes silent.
struct MinimalProtocol;

/// Internal state: tracks whether the single transmission has been sent.
struct MinimalState {
    transmitted: bool,
}

impl Protocol for MinimalProtocol {
    type Config = ();
    type State = MinimalState;
    type Metrics = ();

    fn init(&self, _config: ()) -> (MinimalState, Option<u64>) {
        (MinimalState { transmitted: false }, Some(0))
    }

    fn on_receive(&self, _state: &mut MinimalState, _frame: RxMetadata, _time: u64) -> Option<u64> {
        None
    }

    fn poll_transmit(&self, state: &mut MinimalState, _time: u64) -> Option<Transmission> {
        if state.transmitted {
            return None;
        }
        state.transmitted = true;
        Some(Transmission {
            payload: vec![0x01],
            sf: 7,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency: 868_100_000,
            duration_us: 50_000,
            tx_power_dbm: 14,
        })
    }

    fn update(&self, _state: &mut MinimalState, _time: u64) -> Option<u64> {
        None
    }

    fn metrics(&self, _state: &MinimalState) {}
}

#[test]
fn protocol_init_provides_wake_time() {
    let protocol = MinimalProtocol;
    let (_state, wake_time) = protocol.init(());
    assert!(
        wake_time.is_some(),
        "init() should return Some(wake_time) for a protocol that plans to transmit"
    );
}

#[test]
fn protocol_poll_transmit_once_then_none() {
    let protocol = MinimalProtocol;
    let (mut state, _) = protocol.init(());

    let first = protocol.poll_transmit(&mut state, 0);
    assert!(
        first.is_some(),
        "poll_transmit() should return Some(Transmission) on the first call"
    );

    let second = protocol.poll_transmit(&mut state, 1);
    assert!(
        second.is_none(),
        "poll_transmit() should return None once the single transmission has been sent"
    );
}

#[test]
fn protocol_on_receive_does_not_panic() {
    let protocol = MinimalProtocol;
    let (mut state, _) = protocol.init(());

    let frame = RxMetadata {
        payload: vec![0xAB, 0xCD],
        rssi: -90.0,
        snr: 5.0,
        sf: 7,
        frequency: 868_100_000,
        time: 100,
    };

    // Must not panic.
    let _ = protocol.on_receive(&mut state, frame, 100);
}
