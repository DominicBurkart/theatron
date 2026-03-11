use lora_modulation::{Bandwidth, BaseBandModulationParams, CodingRate, SpreadingFactor};
use lorawan_device::Timings;
use lorawan_device::nb_device::radio::{Event, PhyRxTx, Response, RfConfig, RxQuality, TxConfig};

use theatron::types::Transmission;

#[derive(Debug)]
pub struct SimulatedRadio {
    rx_buf: [u8; 256],
    rx_len: usize,
    pending_tx: Option<Transmission>,
    pending_downlink: Option<Vec<u8>>,
    current_rx_config: Option<RfConfig>,
}

impl SimulatedRadio {
    pub fn new() -> Self {
        Self {
            rx_buf: [0u8; 256],
            rx_len: 0,
            pending_tx: None,
            pending_downlink: None,
            current_rx_config: None,
        }
    }

    pub fn take_pending_tx(&mut self) -> Option<Transmission> {
        self.pending_tx.take()
    }

    pub fn inject_downlink(&mut self, data: Vec<u8>) {
        let len = data.len().min(256);
        self.rx_buf[..len].copy_from_slice(&data[..len]);
        self.rx_len = len;
        self.pending_downlink = Some(data);
    }

    #[allow(dead_code)]
    pub fn has_pending_downlink(&self) -> bool {
        self.pending_downlink.is_some()
    }
}

impl Default for SimulatedRadio {
    fn default() -> Self {
        Self::new()
    }
}

fn sf_to_u8(sf: SpreadingFactor) -> u8 {
    sf.factor() as u8
}

fn bw_to_u32(bw: Bandwidth) -> u32 {
    bw.hz()
}

fn cr_to_u8(cr: CodingRate) -> u8 {
    cr.denom() as u8
}

fn compute_duration_us(bb: &BaseBandModulationParams, payload_len: usize) -> u64 {
    bb.time_on_air_us(Some(8), true, payload_len as u8) as u64
}

impl PhyRxTx for SimulatedRadio {
    type PhyEvent = ();
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
                    rf: RfConfig { frequency, bb },
                    ..
                } = tx_config;
                let payload = buf.to_vec();
                let duration_us = compute_duration_us(&bb, payload.len());
                self.pending_tx = Some(Transmission {
                    payload,
                    sf: sf_to_u8(bb.sf),
                    bandwidth: bw_to_u32(bb.bw),
                    coding_rate: cr_to_u8(bb.cr),
                    frequency,
                    duration_us,
                });
                Ok(Response::TxDone(0))
            }
            Event::RxRequest(rf_config) => {
                self.current_rx_config = Some(rf_config);
                Ok(Response::Rxing)
            }
            Event::CancelRx => {
                self.current_rx_config = None;
                Ok(Response::Idle)
            }
            Event::Phy(()) => {
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
        0
    }

    fn get_rx_window_duration_ms(&self) -> u32 {
        100
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bb() -> BaseBandModulationParams {
        BaseBandModulationParams::new(SpreadingFactor::_7, Bandwidth::_125KHz, CodingRate::_4_5)
    }

    fn make_rf() -> RfConfig {
        RfConfig {
            frequency: 868_100_000,
            bb: make_bb(),
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
    fn inject_downlink_populates_rx_buf() {
        let mut radio = SimulatedRadio::new();
        radio.inject_downlink(vec![0x01, 0x02, 0x03]);
        assert_eq!(radio.get_received_packet(), &[0x01, 0x02, 0x03]);
    }

    #[test]
    fn tx_request_populates_pending_tx() {
        let mut radio = SimulatedRadio::new();
        let tx_config = TxConfig {
            pw: 14,
            rf: make_rf(),
        };
        let payload = [0x01, 0x02, 0x03];
        let result = radio.handle_event(Event::TxRequest(tx_config, &payload));
        assert!(result.is_ok());
        let tx = radio.take_pending_tx().expect("should have pending tx");
        assert_eq!(tx.sf, 7);
        assert_eq!(tx.frequency, 868_100_000);
        assert_eq!(tx.payload, &[0x01, 0x02, 0x03]);
    }

    #[test]
    fn rx_request_then_cancel() {
        let mut radio = SimulatedRadio::new();
        let result = radio.handle_event(Event::RxRequest(make_rf()));
        assert!(matches!(result, Ok(Response::Rxing)));
        let result = radio.handle_event(Event::CancelRx);
        assert!(matches!(result, Ok(Response::Idle)));
    }

    #[test]
    fn phy_with_downlink_returns_rx_done() {
        let mut radio = SimulatedRadio::new();
        radio.inject_downlink(vec![0xAB]);
        let result = radio.handle_event(Event::Phy(()));
        assert!(matches!(result, Ok(Response::RxDone(_))));
    }

    #[test]
    fn phy_without_downlink_returns_idle() {
        let mut radio = SimulatedRadio::new();
        let result = radio.handle_event(Event::Phy(()));
        assert!(matches!(result, Ok(Response::Idle)));
    }

    #[test]
    fn timings_values() {
        let radio = SimulatedRadio::new();
        assert_eq!(radio.get_rx_window_offset_ms(), 0);
        assert_eq!(radio.get_rx_window_duration_ms(), 100);
    }
}
