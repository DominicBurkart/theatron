use lora_modulation::BaseBandModulationParams;
use lorawan_device::Timings;
use lorawan_device::nb_device::radio::{Event, PhyRxTx, Response, RfConfig, RxQuality, TxConfig};

use theatron::types::Transmission;

const RX_WINDOW_DURATION_MS: u32 = 1000;
const RX_WINDOW_OFFSET_MS: i32 = 0;

/// The current operating mode of the simulated radio.
///
/// Mirrors the `RadioMode` sketch in ARCHITECTURE.md so that the TX lifecycle
/// is a proper two-step sequence:
///   1. `TxRequest`  → mode transitions to `Txing`, radio returns `Response::Txing`
///   2. `Phy(SimPhyEvent::TxDone { timestamp_ms })` → mode returns to `Idle`,
///      radio returns `Response::TxDone(timestamp_ms)`
#[derive(Debug, Clone)]
pub enum RadioMode {
    Idle,
    Txing { config: TxConfig },
    Rxing { config: RfConfig },
}

/// Physical-layer events that the simulation delivers to the radio via
/// `Event::Phy(SimPhyEvent::...)`.
///
/// - `TxDone { timestamp_ms }`: the channel has finished transmitting the
///   in-flight frame.  The adapter calls this after the scheduler fires a
///   `TxComplete` event for the node.
/// - `RxDone`: a downlink has been injected into the receive buffer via
///   [`SimulatedRadio::inject_downlink`] and the radio should deliver
///   `Response::RxDone` to the lorawan-device state machine.
#[derive(Debug, Clone)]
pub enum SimPhyEvent {
    TxDone { timestamp_ms: u32 },
    RxDone,
}

#[derive(Debug)]
pub struct SimulatedRadio {
    rx_buf: [u8; 256],
    rx_len: usize,
    mode: RadioMode,
    pending_tx: Option<Transmission>,
    pending_downlink: Option<Vec<u8>>,
    current_rx_config: Option<RfConfig>,
}

impl SimulatedRadio {
    pub fn new() -> Self {
        Self {
            rx_buf: [0u8; 256],
            rx_len: 0,
            mode: RadioMode::Idle,
            pending_tx: None,
            pending_downlink: None,
            current_rx_config: None,
        }
    }

    /// Consume and return the pending outbound transmission, if any.
    ///
    /// Called by `LoRaWanAdapter::poll_transmit` to hand the frame to the
    /// scheduler channel.
    pub fn take_pending_tx(&mut self) -> Option<Transmission> {
        self.pending_tx.take()
    }

    /// Return the air-time duration (µs) of the pending outbound transmission
    /// without consuming it.
    ///
    /// Used by the adapter to compute the TxDone wake-up time while leaving
    /// the `Transmission` available for `poll_transmit`.
    pub fn pending_tx_duration_us(&self) -> Option<u64> {
        self.pending_tx.as_ref().map(|t| t.duration_us)
    }

    pub fn inject_downlink(&mut self, data: Vec<u8>, sf: u8, frequency: u32) -> bool {
        let Some(rx_config) = &self.current_rx_config else {
            return false;
        };
        if rx_config.bb.sf.factor() as u8 != sf {
            return false;
        }
        if rx_config.frequency != frequency {
            return false;
        }
        let len = data.len().min(256);
        self.rx_buf[..len].copy_from_slice(&data[..len]);
        self.rx_len = len;
        self.pending_downlink = Some(data);
        true
    }

    #[allow(dead_code)]
    pub fn has_pending_downlink(&self) -> bool {
        self.pending_downlink.is_some()
    }

    /// Returns a reference to the current radio mode.
    #[allow(dead_code)]
    pub fn mode(&self) -> &RadioMode {
        &self.mode
    }
}

impl Default for SimulatedRadio {
    fn default() -> Self {
        Self::new()
    }
}

fn compute_duration_us(bb: &BaseBandModulationParams, payload_len: usize) -> u64 {
    bb.time_on_air_us(Some(8), true, payload_len as u8) as u64
}

impl PhyRxTx for SimulatedRadio {
    type PhyEvent = SimPhyEvent;
    type PhyError = &'static str;
    type PhyResponse = ();

    const MAX_RADIO_POWER: u8 = 20;
    const ANTENNA_GAIN: i8 = 0;

    fn get_mut_radio(&mut self) -> &mut Self {
        self
    }

    fn get_received_packet(&mut self) -> &mut [u8] {
        &mut self.rx_buf[..self.rx_len]
    }

    fn handle_event(&mut self, event: Event<Self>) -> Result<Response<Self>, Self::PhyError>
    where
        Self: Sized,
    {
        match event {
            Event::TxRequest(tx_config, buf) => {
                let TxConfig {
                    pw,
                    rf: RfConfig { frequency, bb, .. },
                } = tx_config.clone();
                let payload = buf.to_vec();
                let duration_us = compute_duration_us(&bb, payload.len());
                self.pending_tx = Some(Transmission {
                    payload,
                    sf: bb.sf.factor() as u8,
                    bandwidth: bb.bw.hz(),
                    coding_rate: bb.cr.denom() as u8,
                    frequency,
                    duration_us,
                    tx_power_dbm: pw,
                });
                // Two-step TX lifecycle per ARCHITECTURE.md:
                // step 1 — transition to Txing and return Response::Txing.
                // step 2 — caller delivers Phy(SimPhyEvent::TxDone { timestamp_ms })
                //           once the channel has finished the transmission.
                self.mode = RadioMode::Txing { config: tx_config };
                Ok(Response::Txing)
            }
            Event::RxRequest(rf_config) => {
                self.current_rx_config = Some(rf_config.clone());
                self.mode = RadioMode::Rxing { config: rf_config };
                Ok(Response::Rxing)
            }
            Event::CancelRx => {
                self.current_rx_config = None;
                self.mode = RadioMode::Idle;
                Ok(Response::Idle)
            }
            Event::Phy(SimPhyEvent::TxDone { timestamp_ms }) => {
                // Step 2 of the two-step TX lifecycle: the channel has finished
                // transmitting.  Return to Idle and report the TX timestamp.
                self.mode = RadioMode::Idle;
                Ok(Response::TxDone(timestamp_ms))
            }
            Event::Phy(SimPhyEvent::RxDone) => {
                // A downlink was injected via inject_downlink; signal RxDone
                // so lorawan-device can read get_received_packet().
                self.mode = RadioMode::Idle;
                self.current_rx_config = None;
                if self.pending_downlink.take().is_some() {
                    Ok(Response::RxDone(RxQuality::new(-80, 10)))
                } else {
                    Ok(Response::Idle)
                }
            }
        }
    }
}

impl Timings for SimulatedRadio {
    fn get_rx_window_offset_ms(&self) -> i32 {
        RX_WINDOW_OFFSET_MS
    }

    fn get_rx_window_duration_ms(&self) -> u32 {
        RX_WINDOW_DURATION_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lora_modulation::{Bandwidth, CodingRate, SpreadingFactor};

    const TEST_SF: u8 = 7;
    const TEST_FREQ: u32 = 868_100_000;

    fn make_bb() -> BaseBandModulationParams {
        BaseBandModulationParams::new(SpreadingFactor::_7, Bandwidth::_125KHz, CodingRate::_4_5)
    }

    fn make_rf() -> RfConfig {
        RfConfig {
            frequency: TEST_FREQ,
            bb: make_bb(),
            max_payload_len: 255,
        }
    }

    fn make_tx_config() -> TxConfig {
        TxConfig {
            pw: 14,
            rf: make_rf(),
        }
    }

    #[test]
    fn new_radio_no_pending_tx() {
        let mut radio = SimulatedRadio::new();
        assert!(radio.take_pending_tx().is_none());
    }

    #[test]
    fn new_radio_no_pending_downlink() {
        let radio = SimulatedRadio::new();
        assert!(!radio.has_pending_downlink());
    }

    #[test]
    fn new_radio_mode_is_idle() {
        let radio = SimulatedRadio::new();
        assert!(matches!(radio.mode(), RadioMode::Idle));
    }

    #[test]
    fn inject_downlink_requires_rx_mode() {
        let mut radio = SimulatedRadio::new();
        // Without entering RX mode, inject_downlink should return false
        assert!(!radio.inject_downlink(vec![0x01, 0x02, 0x03], TEST_SF, TEST_FREQ));
    }

    #[test]
    fn inject_downlink_populates_rx_buf() {
        let mut radio = SimulatedRadio::new();
        // Enter RX mode first
        let _ = radio.handle_event(Event::RxRequest(make_rf()));
        assert!(radio.inject_downlink(vec![0x01, 0x02, 0x03], TEST_SF, TEST_FREQ));
        assert_eq!(radio.get_received_packet(), &[0x01, 0x02, 0x03]);
    }

    /// TxRequest must return Txing (step 1 of the two-step TX lifecycle).
    #[test]
    fn tx_request_returns_txing() {
        let mut radio = SimulatedRadio::new();
        let payload = [0x01, 0x02, 0x03];
        let result = radio.handle_event(Event::TxRequest(make_tx_config(), &payload));
        assert!(
            matches!(result, Ok(Response::Txing)),
            "TxRequest must return Response::Txing, got {:?}",
            result
        );
    }

    /// After TxRequest the mode must be Txing and pending_tx must be set.
    #[test]
    fn tx_request_sets_txing_mode_and_pending_tx() {
        let mut radio = SimulatedRadio::new();
        let payload = [0x01, 0x02, 0x03];
        let _ = radio.handle_event(Event::TxRequest(make_tx_config(), &payload));
        assert!(
            matches!(radio.mode(), RadioMode::Txing { .. }),
            "mode must be Txing after TxRequest"
        );
        let tx = radio.take_pending_tx().expect("should have pending tx");
        assert_eq!(tx.sf, TEST_SF);
        assert_eq!(tx.frequency, TEST_FREQ);
        assert_eq!(tx.payload, &[0x01, 0x02, 0x03]);
    }

    /// pending_tx_duration_us peeks without consuming.
    #[test]
    fn pending_tx_duration_us_peeks_without_consuming() {
        let mut radio = SimulatedRadio::new();
        assert_eq!(radio.pending_tx_duration_us(), None);
        let payload = [0xAB; 10];
        let _ = radio.handle_event(Event::TxRequest(make_tx_config(), &payload));
        let dur = radio.pending_tx_duration_us();
        assert!(dur.is_some(), "should have a duration after TxRequest");
        // pending_tx must still be intact for poll_transmit
        assert!(radio.take_pending_tx().is_some(), "take must still work after peek");
    }

    /// Phy(TxDone) must return TxDone(timestamp_ms) and restore Idle mode
    /// (step 2 of the two-step TX lifecycle).
    #[test]
    fn phy_tx_done_returns_tx_done_with_timestamp() {
        let mut radio = SimulatedRadio::new();
        let payload = [0xAB];
        let _ = radio.handle_event(Event::TxRequest(make_tx_config(), &payload));
        // simulate scheduler delivering TxDone at t=5000 ms
        let result = radio.handle_event(Event::Phy(SimPhyEvent::TxDone { timestamp_ms: 5000 }));
        assert!(
            matches!(result, Ok(Response::TxDone(5000))),
            "Phy(TxDone) must return Response::TxDone(timestamp_ms), got {:?}",
            result
        );
        assert!(
            matches!(radio.mode(), RadioMode::Idle),
            "mode must return to Idle after TxDone"
        );
    }

    #[test]
    fn rx_request_sets_rxing_mode() {
        let mut radio = SimulatedRadio::new();
        let result = radio.handle_event(Event::RxRequest(make_rf()));
        assert!(matches!(result, Ok(Response::Rxing)));
        assert!(
            matches!(radio.mode(), RadioMode::Rxing { .. }),
            "mode must be Rxing after RxRequest"
        );
    }

    #[test]
    fn rx_request_then_cancel() {
        let mut radio = SimulatedRadio::new();
        let result = radio.handle_event(Event::RxRequest(make_rf()));
        assert!(matches!(result, Ok(Response::Rxing)));
        let result = radio.handle_event(Event::CancelRx);
        assert!(matches!(result, Ok(Response::Idle)));
        assert!(matches!(radio.mode(), RadioMode::Idle));
    }

    #[test]
    fn phy_rx_done_with_downlink_returns_rx_done() {
        let mut radio = SimulatedRadio::new();
        // Enter RX mode and inject downlink
        let _ = radio.handle_event(Event::RxRequest(make_rf()));
        assert!(radio.inject_downlink(vec![0xAB], TEST_SF, TEST_FREQ));
        let result = radio.handle_event(Event::Phy(SimPhyEvent::RxDone));
        assert!(matches!(result, Ok(Response::RxDone(_))));
        assert!(matches!(radio.mode(), RadioMode::Idle));
    }

    #[test]
    fn phy_rx_done_without_downlink_returns_idle() {
        let mut radio = SimulatedRadio::new();
        let result = radio.handle_event(Event::Phy(SimPhyEvent::RxDone));
        assert!(matches!(result, Ok(Response::Idle)));
    }

    #[test]
    fn timings_values() {
        let radio = SimulatedRadio::new();
        assert_eq!(radio.get_rx_window_offset_ms(), 0);
        assert_eq!(radio.get_rx_window_duration_ms(), 1000);
    }
}
