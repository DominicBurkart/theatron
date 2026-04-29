use lorawan_device::nb_device::radio::Event as RadioEvent;
use lorawan_device::nb_device::{Device, Response};
use lorawan_device::{AppSKey, DevAddr, JoinMode, NewSKey};

use theatron::scheduler::NodeHandle;
use theatron::time::SimTime;
use theatron::traits::TrafficModel;
use theatron::types::{NodeId, RxMetadata, Transmission};

use crate::file_fragmenter::FileFragmenter;
use crate::prng::Xorshift64;
use crate::simulated_radio::SimulatedRadio;

const BUF_SIZE: usize = 255;

pub struct LoRaWanAdapter {
    id: NodeId,
    device: Device<SimulatedRadio, Xorshift64, BUF_SIZE>,
    fragmenter: FileFragmenter,
    pending_timeout_ms: Option<u32>,
    tx_start_time: SimTime,
    /// Transmission staged during `on_receive` / `update` so that
    /// `poll_transmit` never needs to reach into the radio directly.
    pending_tx: Option<Transmission>,
}

impl LoRaWanAdapter {
    pub fn new(id: NodeId, fragmenter: FileFragmenter, seed: u64) -> Self {
        let radio = SimulatedRadio::new();
        let rng = Xorshift64::new(seed);
        let region = lorawan_device::region::Configuration::new(lorawan_device::Region::EU868);
        let mut device = Device::new(region, radio, rng);

        let credentials = JoinMode::ABP {
            devaddr: DevAddr::from(id.0),
            appskey: AppSKey::from([id.0 as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            nwkskey: NewSKey::from([id.0 as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]),
        };
        device.join(credentials).expect("ABP join must succeed");
        device.set_datarate(lorawan_device::region::DR::_5);

        Self {
            id,
            device,
            fragmenter,
            pending_timeout_ms: None,
            tx_start_time: 0,
            pending_tx: None,
        }
    }

    fn wake_from_timeout(&self, ms: u32) -> SimTime {
        self.tx_start_time + ms as u64 * 1_000
    }

    /// Harvest any transmission that the radio has prepared and stage it
    /// on the adapter so that `poll_transmit` can drain it without
    /// touching the radio directly.
    fn stage_pending_tx(&mut self) {
        if let Some(tx) = self.device.get_radio().take_pending_tx() {
            self.pending_tx = Some(tx);
        }
    }

    fn try_send_fragment(&mut self, time: SimTime) -> Option<SimTime> {
        if !self.device.ready_to_send_data() {
            return self.fragmenter.next_available_time(time);
        }
        if let Some(payload) = self.fragmenter.next_payload(time) {
            self.tx_start_time = time;
            match self.device.send(&payload, 1, false) {
                Ok(Response::TimeoutRequest(ms)) => {
                    self.pending_timeout_ms = Some(ms);
                    self.stage_pending_tx();
                    Some(self.wake_from_timeout(ms))
                }
                Ok(_) => {
                    self.stage_pending_tx();
                    None
                }
                Err(_) => None,
            }
        } else {
            self.fragmenter.next_available_time(time)
        }
    }
}

impl NodeHandle for LoRaWanAdapter {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        let radio = self.device.get_radio();
        if !radio.inject_downlink(frame.payload.clone(), frame.sf, frame.frequency) {
            return None;
        }
        let result = self
            .device
            .handle_event(lorawan_device::nb_device::Event::RadioEvent(
                RadioEvent::Phy(()),
            ));
        // Stage any transmission the device queued onto the radio before
        // returning, so poll_transmit only needs to drain the adapter field.
        self.stage_pending_tx();
        match result {
            Ok(Response::TimeoutRequest(ms)) => {
                self.pending_timeout_ms = Some(ms);
                Some(self.wake_from_timeout(ms))
            }
            Ok(Response::DownlinkReceived(_)) | Ok(Response::RxComplete) => {
                self.pending_timeout_ms = None;
                None
            }
            Ok(_) => None,
            Err(_) => None,
        }
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        self.pending_tx.take()
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        if let Some(_timeout_ms) = self.pending_timeout_ms.take() {
            let result = self
                .device
                .handle_event(lorawan_device::nb_device::Event::TimeoutFired);
            // Stage any transmission produced by the timeout event before
            // inspecting the response variant.
            self.stage_pending_tx();
            match result {
                Ok(Response::TimeoutRequest(ms)) => {
                    self.pending_timeout_ms = Some(ms);
                    Some(self.wake_from_timeout(ms))
                }
                Ok(Response::RxComplete) | Ok(Response::NoAck) => self.try_send_fragment(time),
                Ok(_) => None,
                Err(_) => None,
            }
        } else {
            self.try_send_fragment(time)
        }
    }
}
