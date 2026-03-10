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
