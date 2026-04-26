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

/// Derive a per-node PRNG seed from a master seed and a node identifier.
///
/// Mixing uses a Knuth multiplicative hash step so that every `(master_seed,
/// node_id)` pair produces a distinct, well-distributed seed:
///
/// ```text
/// per_node_seed = master_seed ^ (node_id.wrapping_mul(0x9e3779b97f4a7c15))
/// ```
///
/// The constant `0x9e3779b97f4a7c15` is the 64-bit fractional part of the
/// golden ratio, a standard choice for Fibonacci hashing.  XOR-ing it with
/// the master seed ensures that two nodes with `node_id = 0` and
/// `node_id = 1` receive seeds that differ by more than one bit flip, making
/// simulations reproducible and per-node sequences independent.
pub fn derive_seed(master_seed: u64, node_id: u64) -> u64 {
    master_seed ^ node_id.wrapping_mul(0x9e3779b97f4a7c15)
}

pub struct LoRaWanAdapter {
    id: NodeId,
    device: Device<SimulatedRadio, Xorshift64, BUF_SIZE>,
    fragmenter: FileFragmenter,
    pending_timeout_ms: Option<u32>,
    tx_start_time: SimTime,
    joined: bool,
}

impl LoRaWanAdapter {
    /// Create a new adapter for `id`.
    ///
    /// The RNG seed used internally is **derived** from `master_seed` and the
    /// numeric value of `node_id` via [`derive_seed`], so each node in a
    /// multi-node simulation gets an independent, reproducible PRNG stream
    /// while the caller only needs to track a single master seed constant.
    pub fn new(id: NodeId, fragmenter: FileFragmenter, master_seed: u64, node_id: u64) -> Self {
        let seed = derive_seed(master_seed, node_id);
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

    fn try_send_fragment(&mut self, time: SimTime) -> Option<SimTime> {
        if !self.joined || !self.device.ready_to_send_data() {
            return self.fragmenter.next_available_time(time);
        }
        if let Some(payload) = self.fragmenter.next_payload(time) {
            self.tx_start_time = time;
            match self.device.send(&payload, 1, false) {
                Ok(Response::TimeoutRequest(ms)) => {
                    self.pending_timeout_ms = Some(ms);
                    Some(self.wake_from_timeout(ms))
                }
                Ok(_) => None,
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
        match self
            .device
            .handle_event(lorawan_device::nb_device::Event::RadioEvent(
                RadioEvent::Phy(()),
            )) {
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
        self.device.get_radio().take_pending_tx()
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        if let Some(_timeout_ms) = self.pending_timeout_ms.take() {
            match self
                .device
                .handle_event(lorawan_device::nb_device::Event::TimeoutFired)
            {
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
