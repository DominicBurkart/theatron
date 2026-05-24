use lorawan_device::nb_device::radio::Event as RadioEvent;
use lorawan_device::nb_device::{Device, Response};
use lorawan_device::{AppSKey, DevAddr, JoinMode, NewSKey};

use theatron::prng::Xorshift64;
use theatron::scheduler::NodeHandle;
use theatron::time::SimTime;
use theatron::traits::TrafficModel;
use theatron::types::{NodeId, RxMetadata, Transmission};

use crate::file_fragmenter::FileFragmenter;
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
    /// Transmission staged during `on_receive` / `update` so that
    /// `poll_transmit` never needs to reach into the radio directly.
    pending_tx: Option<Transmission>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-node seed derivation must be deterministic: the same
    /// `(master_seed, node_id)` pair must always produce the same output.
    /// This is the reproducibility guarantee called out in
    /// ARCHITECTURE.md "Randomness".
    ///
    /// The concrete expected value pins the formula output:
    ///   0xDEAD_BEEF ^ (1u64.wrapping_mul(0x9e3779b97f4a7c15))
    ///   = 0x0000_0000_DEAD_BEEF ^ 0x9e37_79b9_7f4a_7c15
    ///   = 0x9e37_79b9_a1e7_c2fa
    #[test]
    fn derive_seed_is_deterministic() {
        assert_eq!(derive_seed(0xDEAD_BEEF, 1), 0x9e3779b9a1e7c2fa_u64);
        assert_eq!(derive_seed(0, 0), 0);
    }

    /// Distinct node ids under the same master seed must produce distinct
    /// per-node seeds — otherwise two nodes would share a PRNG stream.
    #[test]
    fn derive_seed_distinguishes_node_ids() {
        let master = 0xDEAD_BEEF_1234_5678u64;
        let s1 = derive_seed(master, 1);
        let s2 = derive_seed(master, 2);
        let s3 = derive_seed(master, 3);
        assert_ne!(s1, s2);
        assert_ne!(s2, s3);
        assert_ne!(s1, s3);
    }

    /// Distinct master seeds must produce distinct per-node seeds for the
    /// same node id — otherwise changing the simulation seed would not
    /// change a given node's stream.
    #[test]
    fn derive_seed_distinguishes_master_seeds() {
        assert_ne!(derive_seed(1, 7), derive_seed(2, 7));
    }

    /// Pin the boundary: `node_id == 0` makes the multiplicative term zero,
    /// so `derive_seed` returns the master seed verbatim. This is a known
    /// property — callers that need node 0 to have a distinct stream must
    /// offset node ids by 1, as the `lorawan_file_transfer` example does
    /// (devices use `node_id = 1`, server uses `node_id = 100`).
    #[test]
    fn derive_seed_node_zero_returns_master() {
        assert_eq!(derive_seed(0xABCD, 0), 0xABCD);
        assert_eq!(derive_seed(0, 0), 0);
    }
}
