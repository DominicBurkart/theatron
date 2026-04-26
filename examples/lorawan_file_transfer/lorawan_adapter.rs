use lorawan_device::nb_device::radio::Event as RadioEvent;
use lorawan_device::nb_device::{Device, Response};
use lorawan_device::{AppSKey, DevAddr, JoinMode, NewSKey};

use theatron::scheduler::NodeHandle;
use theatron::time::SimTime;
use theatron::traits::TrafficModel;
use theatron::types::{NodeId, RxMetadata, Transmission};

use crate::file_fragmenter::FileFragmenter;
use crate::prng::Xorshift64;
use crate::simulated_radio::{SimPhyEvent, SimulatedRadio};

const BUF_SIZE: usize = 255;

pub struct LoRaWanAdapter {
    id: NodeId,
    device: Device<SimulatedRadio, Xorshift64, BUF_SIZE>,
    fragmenter: FileFragmenter,
    pending_timeout_ms: Option<u32>,
    /// Absolute simulation time (µs) at which the in-flight TX finishes.
    /// Set when lorawan-device responds with `Response::Txing`; cleared once
    /// `Phy(TxDone)` has been delivered to the device.
    tx_end_time: Option<SimTime>,
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
            tx_end_time: None,
            tx_start_time: 0,
            joined: true,
        }
    }

    fn wake_from_timeout(&self, ms: u32) -> SimTime {
        self.tx_start_time + ms as u64 * 1_000
    }

    /// Compute the simulation timestamp (in ms) at which a TX that started at
    /// `start_us` with duration `duration_us` completes.
    fn tx_done_timestamp_ms(start_us: SimTime, duration_us: u64) -> u32 {
        ((start_us + duration_us) / 1_000) as u32
    }

    fn try_send_fragment(&mut self, time: SimTime) -> Option<SimTime> {
        if !self.joined || !self.device.ready_to_send_data() {
            return self.fragmenter.next_available_time(time);
        }
        if let Some(payload) = self.fragmenter.next_payload(time) {
            self.tx_start_time = time;
            match self.device.send(&payload, 1, false) {
                Ok(Response::Txing) => {
                    // Step 1 of the two-step TX lifecycle: lorawan-device has
                    // handed the frame to SimulatedRadio, which has stored it
                    // in pending_tx.  The scheduler will pick it up via
                    // poll_transmit and fire a TxComplete event at
                    // time + duration_us.  We compute that moment now so we
                    // can schedule a wake-up and deliver TxDone (step 2).
                    if let Some(tx) = self.device.get_radio().take_pending_tx() {
                        let end_us = time + tx.duration_us;
                        self.tx_end_time = Some(end_us);
                        // Put the transmission back so poll_transmit can
                        // hand it to the scheduler channel.
                        // We use a private shim: re-inject it through the
                        // radio's pending_tx field via the public accessor
                        // pattern already provided.
                        self.device.get_radio().set_pending_tx(tx);
                        Some(end_us)
                    } else {
                        None
                    }
                }
                // Defensive: older behaviour or unexpected response.
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
                RadioEvent::Phy(SimPhyEvent::RxDone),
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
        // Step 2 of the two-step TX lifecycle: if we have reached the end of
        // an in-flight TX, deliver Phy(TxDone) to lorawan-device so it can
        // open its RX windows.
        if let Some(end_us) = self.tx_end_time {
            if time >= end_us {
                self.tx_end_time = None;
                let timestamp_ms = (end_us / 1_000) as u32;
                match self
                    .device
                    .handle_event(lorawan_device::nb_device::Event::RadioEvent(
                        RadioEvent::Phy(SimPhyEvent::TxDone { timestamp_ms }),
                    )) {
                    Ok(Response::TimeoutRequest(ms)) => {
                        self.pending_timeout_ms = Some(ms);
                        return Some(self.wake_from_timeout(ms));
                    }
                    Ok(_) => return None,
                    Err(_) => return None,
                }
            }
        }

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
