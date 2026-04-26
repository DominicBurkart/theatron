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
    joined: bool,
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
            joined: true,
        }
    }

    fn wake_from_timeout(&self, ms: u32) -> SimTime {
        self.tx_start_time + ms as u64 * 1_000
    }

    /// Converts a device `Result<Response, _>` into an `Option<SimTime>` and
    /// keeps `pending_timeout_ms` in sync.  A `TimeoutRequest` arms the next
    /// wake-up; every other outcome leaves the field unchanged and returns
    /// `None`.
    fn apply_response<E>(&mut self, result: Result<Response, E>) -> Option<SimTime> {
        match result {
            Ok(Response::TimeoutRequest(ms)) => {
                self.pending_timeout_ms = Some(ms);
                Some(self.wake_from_timeout(ms))
            }
            Ok(_) => None,
            Err(_) => None,
        }
    }

    fn try_send_fragment(&mut self, time: SimTime) -> Option<SimTime> {
        if !self.joined || !self.device.ready_to_send_data() {
            return self.fragmenter.next_available_time(time);
        }
        if let Some(payload) = self.fragmenter.next_payload(time) {
            self.tx_start_time = time;
            self.apply_response(self.device.send(&payload, 1, false))
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
        match result {
            Ok(Response::DownlinkReceived(_)) | Ok(Response::RxComplete) => {
                self.pending_timeout_ms = None;
                None
            }
            other => self.apply_response(other),
        }
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        self.device.get_radio().take_pending_tx()
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        if let Some(_timeout_ms) = self.pending_timeout_ms.take() {
            let result = self
                .device
                .handle_event(lorawan_device::nb_device::Event::TimeoutFired);
            match result {
                Ok(Response::RxComplete) | Ok(Response::NoAck) => self.try_send_fragment(time),
                other => self.apply_response(other),
            }
        } else {
            self.try_send_fragment(time)
        }
    }
}
