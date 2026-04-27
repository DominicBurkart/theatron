use crate::time::SimTime;
use crate::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

pub type CompletedTx = (NodeId, bool, bool, RxMetadata);

/// Default LoRa path loss in dB (free-space + typical indoor attenuation baseline).
pub const LORA_PATH_LOSS_DB: f32 = 100.0;

/// Default LoRa noise floor in dBm (LoRa sensitivity at SF7/125 kHz).
pub const LORA_NOISE_FLOOR_DBM: f32 = -117.0;

/// Default co-channel rejection threshold in dB (LoRa capture effect threshold).
pub const LORA_CO_CHANNEL_REJECTION_DB: f32 = 6.0;

/// Configuration for physical-layer channel parameters.
///
/// Construct with [`ChannelConfig::lora_defaults()`] for LoRa or supply
/// custom values for other protocols.
///
/// # Examples
///
/// ```
/// use theatron::channel::ChannelConfig;
///
/// // LoRa defaults
/// let cfg = ChannelConfig::lora_defaults();
///
/// // Custom 802.15.4 / Zigbee-like parameters
/// let cfg = ChannelConfig {
///     path_loss_db: 80.0,
///     noise_floor_dbm: -100.0,
///     co_channel_rejection_db: 3.0,
/// };
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelConfig {
    /// Path loss applied to every transmission (dB). Higher values mean
    /// the received signal is weaker.
    pub path_loss_db: f32,

    /// Noise floor of the receiver (dBm). Used to compute SNR.
    pub noise_floor_dbm: f32,

    /// Minimum power difference (dB) required for the stronger signal to
    /// survive a co-channel collision via the capture effect.
    pub co_channel_rejection_db: f32,
}

impl ChannelConfig {
    /// Return the LoRa default parameters used by [`Channel::new()`].
    ///
    /// | Parameter               | Value    |
    /// |-------------------------|----------|
    /// | `path_loss_db`          | 100 dB   |
    /// | `noise_floor_dbm`       | -117 dBm |
    /// | `co_channel_rejection_db` | 6 dB   |
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::ChannelConfig;
    /// let cfg = ChannelConfig::lora_defaults();
    /// assert_eq!(cfg.path_loss_db, 100.0);
    /// assert_eq!(cfg.noise_floor_dbm, -117.0);
    /// assert_eq!(cfg.co_channel_rejection_db, 6.0);
    /// ```
    pub fn lora_defaults() -> Self {
        Self {
            path_loss_db: LORA_PATH_LOSS_DB,
            noise_floor_dbm: LORA_NOISE_FLOOR_DBM,
            co_channel_rejection_db: LORA_CO_CHANNEL_REJECTION_DB,
        }
    }
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self::lora_defaults()
    }
}

struct ActiveTransmission {
    sender: NodeId,
    payload: Vec<u8>,
    sf: u8,
    #[allow(dead_code)]
    bandwidth: u32,
    frequency: u32,
    start: SimTime,
    end: SimTime,
    collided: bool,
    tx_power_dbm: i8,
    captured: bool,
}

/// A simulated wireless channel with collision detection.
///
/// Physical-layer parameters are controlled by [`ChannelConfig`]. Use
/// [`Channel::new()`] for LoRa defaults or [`Channel::with_config()`] to
/// supply custom parameters for other protocols.
pub struct Channel {
    active: Vec<ActiveTransmission>,
    completed: Vec<ActiveTransmission>,
    config: ChannelConfig,
}

impl Channel {
    /// Create a new empty channel using LoRa default parameters.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::Channel;
    /// let ch = Channel::new();
    /// ```
    pub fn new() -> Self {
        Self::with_config(ChannelConfig::lora_defaults())
    }

    /// Create a new channel with the given [`ChannelConfig`].
    ///
    /// This is the primary entry point for non-LoRa protocols.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::{Channel, ChannelConfig};
    ///
    /// // Simulate a short-range, low-noise environment (e.g. 802.15.4)
    /// let ch = Channel::with_config(ChannelConfig {
    ///     path_loss_db: 80.0,
    ///     noise_floor_dbm: -100.0,
    ///     co_channel_rejection_db: 3.0,
    /// });
    /// ```
    pub fn with_config(config: ChannelConfig) -> Self {
        Self {
            active: Vec::new(),
            completed: Vec::new(),
            config,
        }
    }

    /// Return a reference to the channel's physical-layer configuration.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::{Channel, ChannelConfig};
    /// let ch = Channel::new();
    /// assert_eq!(ch.config().path_loss_db, 100.0);
    /// ```
    pub fn config(&self) -> &ChannelConfig {
        &self.config
    }

    pub fn compute_rssi(&self, tx_power_dbm: i8) -> f32 {
        tx_power_dbm as f32 - self.config.path_loss_db
    }

    pub fn compute_snr(&self, rssi: f32) -> f32 {
        rssi - self.config.noise_floor_dbm
    }

    /// Begin a transmission on the channel, returning a `TransmissionStarted` event.
    ///
    /// Collisions are detected immediately: if the new transmission overlaps in time with
    /// an existing active transmission on the same SF and frequency, both are marked collided.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::Channel;
    /// use theatron::types::{NodeId, Transmission};
    ///
    /// let mut ch = Channel::new();
    /// let tx = Transmission {
    ///     payload: vec![0x01],
    ///     sf: 7,
    ///     bandwidth: 125_000,
    ///     coding_rate: 5,
    ///     frequency: 868_100_000,
    ///     duration_us: 50_000,
    ///     tx_power_dbm: 14,
    /// };
    /// let event = ch.begin_transmission(NodeId(1), &tx, 0);
    /// ```
    pub fn begin_transmission(
        &mut self,
        sender: NodeId,
        tx: &Transmission,
        time: SimTime,
    ) -> ChannelEvent {
        let end = time + tx.duration_us;
        let mut new_collided = false;
        let mut new_captured = false;
        for active in &mut self.active {
            if overlaps(active.start, active.end, time, end)
                && active.frequency == tx.frequency
                && active.sf == tx.sf
            {
                let delta = tx.tx_power_dbm as f32 - active.tx_power_dbm as f32;
                if delta >= self.config.co_channel_rejection_db {
                    active.collided = true;
                    new_captured = true;
                } else if delta <= -self.config.co_channel_rejection_db {
                    new_collided = true;
                    active.captured = true;
                } else {
                    active.collided = true;
                    active.captured = false;
                    new_collided = true;
                }
            }
        }
        self.active.push(ActiveTransmission {
            sender,
            payload: tx.payload.clone(),
            sf: tx.sf,
            bandwidth: tx.bandwidth,
            frequency: tx.frequency,
            start: time,
            end,
            collided: new_collided,
            tx_power_dbm: tx.tx_power_dbm,
            captured: new_captured && !new_collided,
        });
        ChannelEvent::TransmissionStarted {
            sender,
            sf: tx.sf,
            frequency: tx.frequency,
            time,
        }
    }

    /// Move all active transmissions that ended at or before `time` to the completed list,
    /// returning a `TransmissionCompleted` event for each.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::Channel;
    /// use theatron::types::{NodeId, Transmission};
    ///
    /// let mut ch = Channel::new();
    /// let tx = Transmission {
    ///     payload: vec![0x01],
    ///     sf: 7,
    ///     bandwidth: 125_000,
    ///     coding_rate: 5,
    ///     frequency: 868_100_000,
    ///     duration_us: 50_000,
    ///     tx_power_dbm: 14,
    /// };
    /// ch.begin_transmission(NodeId(1), &tx, 0);
    /// let events = ch.resolve_at(50_000);
    /// assert_eq!(events.len(), 1);
    /// ```
    pub fn resolve_at(&mut self, time: SimTime) -> Vec<ChannelEvent> {
        let mut events = Vec::new();
        let mut remaining = Vec::new();
        for tx in self.active.drain(..) {
            if tx.end <= time {
                events.push(ChannelEvent::TransmissionCompleted {
                    sender: tx.sender,
                    time: tx.end,
                    collided: tx.collided,
                });
                self.completed.push(tx);
            } else {
                remaining.push(tx);
            }
        }
        self.active = remaining;
        events
    }

    /// Return all completed, non-collided transmissions as received frames.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::Channel;
    /// use theatron::types::{NodeId, Transmission};
    ///
    /// let mut ch = Channel::new();
    /// let tx = Transmission {
    ///     payload: vec![0x42],
    ///     sf: 7,
    ///     bandwidth: 125_000,
    ///     coding_rate: 5,
    ///     frequency: 868_100_000,
    ///     duration_us: 50_000,
    ///     tx_power_dbm: 14,
    /// };
    /// ch.begin_transmission(NodeId(1), &tx, 0);
    /// ch.resolve_at(50_000);
    /// let received = ch.deliver_to(50_000);
    /// assert_eq!(received.len(), 1);
    /// assert_eq!(received[0].payload, vec![0x42]);
    /// ```
    pub fn deliver_to(&self, time: SimTime) -> Vec<RxMetadata> {
        self.completed
            .iter()
            .filter(|tx| tx.end <= time && !tx.collided)
            .map(|tx| RxMetadata {
                payload: tx.payload.clone(),
                rssi: self.compute_rssi(tx.tx_power_dbm),
                snr: self.compute_snr(self.compute_rssi(tx.tx_power_dbm)),
                sf: tx.sf,
                frequency: tx.frequency,
                time: tx.end,
            })
            .collect()
    }

    /// Drain and return all completed transmissions as `CompletedTx` tuples.
    ///
    /// Each entry is `(sender, collided, captured, RxMetadata)`. RSSI and SNR
    /// are computed from the transmission power and channel parameters.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::Channel;
    /// use theatron::types::{NodeId, Transmission};
    ///
    /// let mut ch = Channel::new();
    /// let tx = Transmission {
    ///     payload: vec![0x01],
    ///     sf: 7,
    ///     bandwidth: 125_000,
    ///     coding_rate: 5,
    ///     frequency: 868_100_000,
    ///     duration_us: 50_000,
    ///     tx_power_dbm: 14,
    /// };
    /// ch.begin_transmission(NodeId(1), &tx, 0);
    /// ch.resolve_at(50_000);
    /// let completed = ch.drain_completed();
    /// assert_eq!(completed.len(), 1);
    /// assert_eq!(completed[0].0, NodeId(1));
    /// assert!(!completed[0].1);
    /// ```
    pub fn drain_completed(&mut self) -> Vec<CompletedTx> {
        let path_loss_db = self.config.path_loss_db;
        let noise_floor_dbm = self.config.noise_floor_dbm;
        self.completed
            .drain(..)
            .map(|tx| {
                let rssi = tx.tx_power_dbm as f32 - path_loss_db;
                let snr = rssi - noise_floor_dbm;
                let metadata = RxMetadata {
                    payload: tx.payload,
                    rssi,
                    snr,
                    sf: tx.sf,
                    frequency: tx.frequency,
                    time: tx.end,
                };
                (tx.sender, tx.collided, tx.captured, metadata)
            })
            .collect()
    }
}

impl Default for Channel {
    fn default() -> Self {
        Self::new()
    }
}

fn overlaps(a_start: SimTime, a_end: SimTime, b_start: SimTime, b_end: SimTime) -> bool {
    a_start < b_end && b_start < a_end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Transmission;
    use proptest::prelude::*;

    fn make_tx(sf: u8, frequency: u32, duration_us: u64) -> Transmission {
        make_tx_power(sf, frequency, duration_us, 14)
    }

    fn make_tx_power(sf: u8, frequency: u32, duration_us: u64, tx_power_dbm: i8) -> Transmission {
        Transmission {
            payload: vec![0x01, 0x02],
            sf,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency,
            duration_us,
            tx_power_dbm,
        }
    }

    #[test]
    fn default_creates_empty_channel() {
        let mut ch = Channel::default();
        let tx = make_tx(7, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx, 0);
        ch.resolve_at(50_000);
        assert_eq!(ch.drain_completed().len(), 1);
    }

    #[test]
    fn single_transmission_delivers() {
        let mut ch = Channel::new();
        let tx = make_tx(7, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx, 0);
        ch.resolve_at(50_000);
        let delivered = ch.deliver_to(50_000);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].payload, vec![0x01, 0x02]);
    }

    #[test]
    fn same_sf_same_freq_equal_power_both_collide() {
        let mut ch = Channel::new();
        let tx1 = make_tx(7, 868_100_000, 50_000);
        let tx2 = make_tx(7, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.begin_transmission(NodeId(2), &tx2, 10_000);
        ch.resolve_at(60_000);
        let delivered = ch.deliver_to(60_000);
        assert_eq!(delivered.len(), 0);
    }

    #[test]
    fn different_sf_no_collision() {
        let mut ch = Channel::new();
        let tx1 = make_tx(7, 868_100_000, 50_000);
        let tx2 = make_tx(8, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.begin_transmission(NodeId(2), &tx2, 10_000);
        ch.resolve_at(60_000);
        let delivered = ch.deliver_to(60_000);
        assert_eq!(delivered.len(), 2);
    }

    #[test]
    fn different_frequency_no_collision() {
        let mut ch = Channel::new();
        let tx1 = make_tx(7, 868_100_000, 50_000);
        let tx2 = make_tx(7, 868_300_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.begin_transmission(NodeId(2), &tx2, 10_000);
        ch.resolve_at(60_000);
        let delivered = ch.deliver_to(60_000);
        assert_eq!(delivered.len(), 2);
    }

    #[test]
    fn non_overlapping_transmissions_no_collision() {
        let mut ch = Channel::new();
        let tx1 = make_tx(7, 868_100_000, 50_000);
        let tx2 = make_tx(7, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.resolve_at(50_000);
        ch.drain_completed();
        ch.begin_transmission(NodeId(2), &tx2, 60_000);
        ch.resolve_at(110_000);
        let delivered = ch.deliver_to(110_000);
        assert_eq!(delivered.len(), 1);
    }

    #[test]
    fn exactly_adjacent_no_collision() {
        let mut ch = Channel::new();
        let tx1 = make_tx(7, 868_100_000, 50_000);
        let tx2 = make_tx(7, 868_100_000, 50_000);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.resolve_at(50_000);
        ch.drain_completed();
        ch.begin_transmission(NodeId(2), &tx2, 50_000);
        ch.resolve_at(100_000);
        let completed = ch.drain_completed();
        assert!(completed.iter().all(|(_, collided, _, _)| !collided));
    }

    #[test]
    fn three_way_collision_marks_all() {
        let mut ch = Channel::new();
        for i in 1u32..=3 {
            let tx = make_tx(7, 868_100_000, 50_000);
            ch.begin_transmission(NodeId(i), &tx, 10_000 * (i as u64 - 1));
        }
        ch.resolve_at(70_000);
        let completed = ch.drain_completed();
        assert_eq!(completed.len(), 3);
        assert!(completed.iter().all(|(_, collided, _, _)| *collided));
    }

    #[test]
    fn stronger_signal_survives_capture() {
        let mut ch = Channel::new();
        let strong = make_tx_power(7, 868_100_000, 50_000, 20);
        let weak = make_tx_power(7, 868_100_000, 50_000, 14);
        ch.begin_transmission(NodeId(1), &strong, 0);
        ch.begin_transmission(NodeId(2), &weak, 10_000);
        ch.resolve_at(60_000);
        let delivered = ch.deliver_to(60_000);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].payload, vec![0x01, 0x02]);
    }

    #[test]
    fn weaker_signal_lost_in_capture() {
        let mut ch = Channel::new();
        let strong = make_tx_power(7, 868_100_000, 50_000, 20);
        let weak = make_tx_power(7, 868_100_000, 50_000, 14);
        ch.begin_transmission(NodeId(1), &strong, 0);
        ch.begin_transmission(NodeId(2), &weak, 10_000);
        ch.resolve_at(60_000);
        let completed = ch.drain_completed();
        let strong_entry = completed
            .iter()
            .find(|(id, _, _, _)| *id == NodeId(1))
            .unwrap();
        let weak_entry = completed
            .iter()
            .find(|(id, _, _, _)| *id == NodeId(2))
            .unwrap();
        assert!(!strong_entry.1, "strong should not be collided");
        assert!(strong_entry.2, "strong should be captured");
        assert!(weak_entry.1, "weak should be collided");
    }

    #[test]
    fn just_below_threshold_both_collide() {
        let mut ch = Channel::new();
        let tx1 = make_tx_power(7, 868_100_000, 50_000, 14);
        let tx2 = make_tx_power(7, 868_100_000, 50_000, 9);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.begin_transmission(NodeId(2), &tx2, 10_000);
        ch.resolve_at(60_000);
        let delivered = ch.deliver_to(60_000);
        assert_eq!(delivered.len(), 0, "delta=5 < threshold=6 → both collide");
    }

    #[test]
    fn exactly_at_threshold_stronger_survives() {
        let mut ch = Channel::new();
        let tx1 = make_tx_power(7, 868_100_000, 50_000, 20);
        let tx2 = make_tx_power(7, 868_100_000, 50_000, 14);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.begin_transmission(NodeId(2), &tx2, 10_000);
        ch.resolve_at(60_000);
        let delivered = ch.deliver_to(60_000);
        assert_eq!(
            delivered.len(),
            1,
            "delta=6 == threshold=6 → stronger survives"
        );
    }

    #[test]
    fn three_way_strongest_wins() {
        let mut ch = Channel::new();
        let strong = make_tx_power(7, 868_100_000, 50_000, 20);
        let medium = make_tx_power(7, 868_100_000, 50_000, 14);
        let weak = make_tx_power(7, 868_100_000, 50_000, 8);
        ch.begin_transmission(NodeId(1), &strong, 0);
        ch.begin_transmission(NodeId(2), &medium, 5_000);
        ch.begin_transmission(NodeId(3), &weak, 10_000);
        ch.resolve_at(60_000);
        let delivered = ch.deliver_to(60_000);
        assert_eq!(
            delivered.len(),
            1,
            "only strongest survives three-way collision"
        );
        let completed = ch.drain_completed();
        let strong_entry = completed
            .iter()
            .find(|(id, _, _, _)| *id == NodeId(1))
            .unwrap();
        assert!(strong_entry.2, "strongest should be marked captured");
    }

    #[test]
    fn configurable_threshold() {
        let mut ch = Channel::with_config(ChannelConfig {
            co_channel_rejection_db: 10.0,
            ..ChannelConfig::lora_defaults()
        });
        let tx1 = make_tx_power(7, 868_100_000, 50_000, 20);
        let tx2 = make_tx_power(7, 868_100_000, 50_000, 14);
        ch.begin_transmission(NodeId(1), &tx1, 0);
        ch.begin_transmission(NodeId(2), &tx2, 10_000);
        ch.resolve_at(60_000);
        let delivered = ch.deliver_to(60_000);
        assert_eq!(delivered.len(), 0, "delta=6 < threshold=10 → both collide");
    }

    #[test]
    fn rssi_derived_from_tx_power() {
        let mut ch = Channel::new();
        let tx = make_tx_power(7, 868_100_000, 50_000, 14);
        ch.begin_transmission(NodeId(1), &tx, 0);
        ch.resolve_at(50_000);
        let delivered = ch.deliver_to(50_000);
        assert_eq!(delivered.len(), 1);
        assert!((delivered[0].rssi - (14.0_f32 - 100.0)).abs() < 0.001);
        assert!((delivered[0].snr - (-86.0_f32 - (-117.0))).abs() < 0.001);
    }

    // --- ChannelConfig tests ---

    #[test]
    fn channel_config_lora_defaults_matches_constants() {
        let cfg = ChannelConfig::lora_defaults();
        assert_eq!(cfg.path_loss_db, LORA_PATH_LOSS_DB);
        assert_eq!(cfg.noise_floor_dbm, LORA_NOISE_FLOOR_DBM);
        assert_eq!(cfg.co_channel_rejection_db, LORA_CO_CHANNEL_REJECTION_DB);
    }

    #[test]
    fn channel_config_default_is_lora_defaults() {
        assert_eq!(ChannelConfig::default(), ChannelConfig::lora_defaults());
    }

    #[test]
    fn with_config_exposes_config_accessor() {
        let cfg = ChannelConfig {
            path_loss_db: 70.0,
            noise_floor_dbm: -95.0,
            co_channel_rejection_db: 3.0,
        };
        let ch = Channel::with_config(cfg.clone());
        assert_eq!(*ch.config(), cfg);
    }

    /// A channel with lower path loss (shorter range / indoor) produces a higher
    /// RSSI for the same transmit power.
    #[test]
    fn low_path_loss_config_produces_higher_rssi() {
        let low_loss_cfg = ChannelConfig {
            path_loss_db: 60.0,
            noise_floor_dbm: -100.0,
            co_channel_rejection_db: 3.0,
        };
        let mut ch_lora = Channel::new(); // path_loss = 100 dB
        let mut ch_short_range = Channel::with_config(low_loss_cfg);

        let tx = make_tx_power(7, 868_100_000, 50_000, 14);

        for ch in [&mut ch_lora, &mut ch_short_range] {
            ch.begin_transmission(NodeId(1), &tx, 0);
            ch.resolve_at(50_000);
        }

        let lora_rx = ch_lora.deliver_to(50_000);
        let short_rx = ch_short_range.deliver_to(50_000);

        assert_eq!(lora_rx.len(), 1);
        assert_eq!(short_rx.len(), 1);
        assert!(
            short_rx[0].rssi > lora_rx[0].rssi,
            "lower path loss must yield higher RSSI: short_range={} lora={}",
            short_rx[0].rssi,
            lora_rx[0].rssi,
        );
    }

    /// A channel with a lower noise floor produces a higher SNR for the same signal.
    #[test]
    fn lower_noise_floor_produces_higher_snr() {
        let quiet_cfg = ChannelConfig {
            path_loss_db: 100.0,
            noise_floor_dbm: -130.0, // quieter than LoRa default of -117 dBm
            co_channel_rejection_db: 6.0,
        };
        let mut ch_lora = Channel::new();
        let mut ch_quiet = Channel::with_config(quiet_cfg);

        let tx = make_tx_power(7, 868_100_000, 50_000, 14);

        for ch in [&mut ch_lora, &mut ch_quiet] {
            ch.begin_transmission(NodeId(1), &tx, 0);
            ch.resolve_at(50_000);
        }

        let lora_rx = ch_lora.deliver_to(50_000);
        let quiet_rx = ch_quiet.deliver_to(50_000);

        assert_eq!(lora_rx.len(), 1);
        assert_eq!(quiet_rx.len(), 1);
        assert!(
            quiet_rx[0].snr > lora_rx[0].snr,
            "lower noise floor must yield higher SNR: quiet={} lora={}",
            quiet_rx[0].snr,
            lora_rx[0].snr,
        );
    }

    /// A channel with a higher capture threshold behaves more conservatively:
    /// a power delta that would survive capture on the LoRa channel causes both
    /// signals to collide instead.
    #[test]
    fn high_capture_threshold_prevents_capture_where_lora_would_survive() {
        // delta = 6 dB exactly meets the LoRa threshold → strong survives on LoRa
        let mut ch_lora = Channel::new(); // threshold = 6 dB
        let mut ch_strict = Channel::with_config(ChannelConfig {
            path_loss_db: 100.0,
            noise_floor_dbm: -117.0,
            co_channel_rejection_db: 10.0, // stricter: 6 dB delta not enough
        });

        let strong = make_tx_power(7, 868_100_000, 50_000, 20);
        let weak = make_tx_power(7, 868_100_000, 50_000, 14);

        for ch in [&mut ch_lora, &mut ch_strict] {
            ch.begin_transmission(NodeId(1), &strong, 0);
            ch.begin_transmission(NodeId(2), &weak, 10_000);
            ch.resolve_at(60_000);
        }

        let lora_rx = ch_lora.deliver_to(60_000);
        let strict_rx = ch_strict.deliver_to(60_000);

        assert_eq!(
            lora_rx.len(),
            1,
            "LoRa threshold=6: strong signal must survive"
        );
        assert_eq!(
            strict_rx.len(),
            0,
            "strict threshold=10: both collide at delta=6"
        );
    }

    proptest! {
        #[test]
        fn n_non_overlapping_txs_never_collide(n in 2usize..20) {
            let mut ch = Channel::new();
            let duration = 50_000u64;
            let gap = 10_000u64;
            let mut all_clean = true;
            let mut t = 0u64;
            for _ in 0..n {
                let tx = make_tx(7, 868_100_000, duration);
                ch.begin_transmission(NodeId(1), &tx, t);
                ch.resolve_at(t + duration);
                let completed = ch.drain_completed();
                if completed.iter().any(|(_, collided, _, _)| *collided) {
                    all_clean = false;
                    break;
                }
                t += duration + gap;
            }
            prop_assert!(all_clean);
        }

        #[test]
        fn n_simultaneous_same_sf_freq_all_collide(n in 2usize..10) {
            let mut ch = Channel::new();
            let duration = 50_000u64;
            for i in 0..n {
                let tx = make_tx(7, 868_100_000, duration);
                ch.begin_transmission(NodeId(i as u32), &tx, 0);
            }
            ch.resolve_at(duration);
            let completed = ch.drain_completed();
            prop_assert_eq!(completed.len(), n);
            prop_assert!(completed.iter().all(|(_, collided, _, _)| *collided));
        }

        #[test]
        fn different_sf_never_collide(sf_a in 7u8..13u8, sf_b in 7u8..13u8) {
            prop_assume!(sf_a != sf_b);
            let mut ch = Channel::new();
            let duration = 50_000u64;
            let tx_a = make_tx(sf_a, 868_100_000, duration);
            let tx_b = make_tx(sf_b, 868_100_000, duration);
            ch.begin_transmission(NodeId(1), &tx_a, 0);
            ch.begin_transmission(NodeId(2), &tx_b, 0);
            ch.resolve_at(duration);
            let completed = ch.drain_completed();
            prop_assert!(completed.iter().all(|(_, collided, _, _)| !collided));
        }
    }
}
