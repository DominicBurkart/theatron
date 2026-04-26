/// Shared `Transmission` factory helpers used across integration-test files.
///
/// All helpers produce a `Transmission` with the standard LoRa defaults:
///   - bandwidth:    125 000 Hz
///   - coding_rate:  5
///
/// Bring the helpers you need into scope with:
///
/// ```rust,ignore
/// #[path = "helpers.rs"]
/// mod helpers;
/// use helpers::{make_tx, make_tx_power};
/// ```
use theatron::types::Transmission;

/// Build a `Transmission` with a caller-supplied payload and tx power.
///
/// This is the most general factory; all other helpers delegate to it.
pub fn make_tx_full(
    payload: Vec<u8>,
    sf: u8,
    frequency: u32,
    duration_us: u64,
    tx_power_dbm: i8,
) -> Transmission {
    Transmission {
        payload,
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency,
        duration_us,
        tx_power_dbm,
    }
}

/// Build a `Transmission` with a caller-supplied payload and fixed tx power (14 dBm).
///
/// Used by `tests/aloha.rs` (was `make_tx`).
pub fn make_tx(payload: Vec<u8>, sf: u8, frequency: u32, duration_us: u64) -> Transmission {
    make_tx_full(payload, sf, frequency, duration_us, 14)
}

/// Build a `Transmission` with a caller-supplied payload and caller-supplied tx power.
///
/// Used by `tests/aloha.rs` (was `make_tx_power`).
pub fn make_tx_power(
    payload: Vec<u8>,
    sf: u8,
    frequency: u32,
    duration_us: u64,
    tx_power_dbm: i8,
) -> Transmission {
    make_tx_full(payload, sf, frequency, duration_us, tx_power_dbm)
}

/// Build a `Transmission` with a single-byte payload (`0xAA`) and caller-supplied tx power.
///
/// Used by `tests/core_coverage.rs` (was `tx`).
pub fn tx(sf: u8, freq: u32, dur: u64, power: i8) -> Transmission {
    make_tx_full(vec![0xAA], sf, freq, dur, power)
}

/// Build a `Transmission` with a caller-supplied payload and caller-supplied tx power.
///
/// Alias of [`make_tx_power`]; retained as a distinct name to match the call
/// sites in `tests/core_coverage.rs` (was `tx_with_payload`).
pub fn tx_with_payload(payload: Vec<u8>, sf: u8, freq: u32, dur: u64, power: i8) -> Transmission {
    make_tx_full(payload, sf, freq, dur, power)
}

/// Build a `Transmission` with a single-byte payload (`0xAB`) and fixed tx power (14 dBm).
///
/// Used by `src/scheduler.rs` #[cfg(test)] (was the local `make_tx`).
pub fn make_tx_default(sf: u8, frequency: u32, duration_us: u64) -> Transmission {
    make_tx_full(vec![0xAB], sf, frequency, duration_us, 14)
}
