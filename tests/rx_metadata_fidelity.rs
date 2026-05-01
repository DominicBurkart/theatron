//! Pin down the contract between `Transmission` (input) and `RxMetadata`
//! (delivered to receivers) when a frame travels through the scheduler.
//!
//! These invariants are implied by `Channel::drain_completed` (in
//! `src/channel.rs`) and `Scheduler::deliver_completed_to_nodes` (in
//! `src/scheduler.rs`), but were previously only spot-checked at the
//! channel-unit level (RSSI/SNR derivation) and not end-to-end through
//! the scheduler. A regression in either layer — payload truncation, SF
//! mis-assignment, frequency drop, RSSI mis-derivation — would silently
//! break every protocol adapter that filters by frequency or branches on
//! signal quality.
//!
//! Invariants verified:
//!
//! 1. `RxMetadata.payload`, `sf`, and `frequency` are byte-equal to the
//!    sender's `Transmission`.
//! 2. `RxMetadata.rssi == tx_power_dbm - path_loss_db` and
//!    `RxMetadata.snr == rssi - noise_floor_dbm`, using the channel's
//!    `ChannelConfig`.
//! 3. `RxMetadata.time` equals the TX completion time
//!    (`tx_start + duration_us`).
//! 4. Captured frames are delivered with metadata matching the *strong*
//!    sender, not the weak/colliding one.
//! 5. Interferer-injected transmissions reach receivers with the same
//!    metadata fidelity as node-originated transmissions.

use std::cell::RefCell;
use std::rc::Rc;

use theatron::channel::{Channel, ChannelConfig};
use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::SimTime;
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_tx(payload: Vec<u8>, sf: u8, freq: u32, dur: u64, power: i8) -> Transmission {
    Transmission {
        payload,
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency: freq,
        duration_us: dur,
        tx_power_dbm: power,
    }
}

/// A receiver that records every `RxMetadata` it sees.
struct RecordingReceiver {
    id: NodeId,
    received: Rc<RefCell<Vec<RxMetadata>>>,
}

impl NodeHandle for RecordingReceiver {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        self.received.borrow_mut().push(frame);
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        None
    }
    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

/// One-shot transmitter.
struct OneShotSender {
    id: NodeId,
    tx: Option<Transmission>,
}

impl NodeHandle for OneShotSender {
    fn node_id(&self) -> NodeId {
        self.id
    }
    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }
    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        self.tx.take()
    }
    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

/// Interferer that injects exactly one frame on its first poll.
struct OneShotInterferer {
    tx: Option<Transmission>,
}

impl InterferenceSource for OneShotInterferer {
    fn observe(&mut self, _event: &ChannelEvent, _time: SimTime) {}
    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        self.tx.take()
    }
    fn next_poll_time(&self, _current_time: SimTime) -> Option<SimTime> {
        None
    }
}

// ---------------------------------------------------------------------------
// Invariants 1, 2, 3: payload/sf/frequency/rssi/snr/time fidelity
// ---------------------------------------------------------------------------

#[test]
fn delivered_frame_matches_transmitted_payload_sf_and_frequency() {
    let received = Rc::new(RefCell::new(Vec::new()));

    let mut sched = Scheduler::new(500_000);
    let payload = vec![0x01, 0x02, 0x03, 0x04, 0x05];
    let sender_tx = make_tx(payload.clone(), 9, 868_300_000, 75_000, 14);

    sched.add_node(
        Box::new(OneShotSender {
            id: NodeId(1),
            tx: Some(sender_tx.clone()),
        }),
        Some(0),
    );
    sched.add_node(
        Box::new(RecordingReceiver {
            id: NodeId(2),
            received: Rc::clone(&received),
        }),
        None,
    );
    sched.run();

    let frames = received.borrow();
    assert_eq!(frames.len(), 1, "exactly one frame should be delivered");
    let f = &frames[0];
    assert_eq!(f.payload, payload);
    assert_eq!(f.sf, sender_tx.sf);
    assert_eq!(f.frequency, sender_tx.frequency);

    // RSSI/SNR derived from default LoRa ChannelConfig:
    //   rssi = tx_power_dbm - path_loss_db = 14 - 100 = -86
    //   snr  = rssi - noise_floor_dbm     = -86 - (-117) = 31
    let cfg = ChannelConfig::lora_defaults();
    let expected_rssi = sender_tx.tx_power_dbm as f32 - cfg.path_loss_db;
    let expected_snr = expected_rssi - cfg.noise_floor_dbm;
    assert!(
        (f.rssi - expected_rssi).abs() < 1e-3,
        "rssi: got {} expected {}",
        f.rssi,
        expected_rssi
    );
    assert!(
        (f.snr - expected_snr).abs() < 1e-3,
        "snr: got {} expected {}",
        f.snr,
        expected_snr
    );
}

#[test]
fn delivered_frame_time_equals_tx_completion_time() {
    let received = Rc::new(RefCell::new(Vec::new()));
    let mut sched = Scheduler::new(500_000);

    // Sender wakes at t=0 (initial wake), transmits a 50ms frame -> completion at 50_000.
    let tx_dur = 50_000u64;
    sched.add_node(
        Box::new(OneShotSender {
            id: NodeId(1),
            tx: Some(make_tx(vec![0xAB], 7, 868_100_000, tx_dur, 14)),
        }),
        Some(0),
    );
    sched.add_node(
        Box::new(RecordingReceiver {
            id: NodeId(2),
            received: Rc::clone(&received),
        }),
        None,
    );
    sched.run();

    let frames = received.borrow();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].time, tx_dur,
        "RxMetadata.time must equal the TX completion time"
    );
}

#[test]
fn rssi_and_snr_track_custom_channel_config() {
    let received = Rc::new(RefCell::new(Vec::new()));

    // 802.15.4-like config: 80 dB path loss, -100 dBm noise floor.
    let cfg = ChannelConfig {
        path_loss_db: 80.0,
        noise_floor_dbm: -100.0,
        co_channel_rejection_db: 3.0,
    };
    let mut sched = Scheduler::with_channel(500_000, Channel::with_config(cfg.clone()));
    sched.add_node(
        Box::new(OneShotSender {
            id: NodeId(1),
            tx: Some(make_tx(vec![0x01], 7, 868_100_000, 50_000, 0)),
        }),
        Some(0),
    );
    sched.add_node(
        Box::new(RecordingReceiver {
            id: NodeId(2),
            received: Rc::clone(&received),
        }),
        None,
    );
    sched.run();

    let frames = received.borrow();
    assert_eq!(frames.len(), 1);
    // tx_power=0, path_loss=80 -> rssi=-80; noise_floor=-100 -> snr=20
    assert!((frames[0].rssi - (-80.0)).abs() < 1e-3);
    assert!((frames[0].snr - 20.0).abs() < 1e-3);
}

// ---------------------------------------------------------------------------
// Invariant 4: capture metadata fidelity
// ---------------------------------------------------------------------------

#[test]
fn captured_frame_carries_strong_senders_metadata() {
    let received = Rc::new(RefCell::new(Vec::new()));
    let mut sched = Scheduler::new(500_000);

    let strong_payload = vec![0xAA, 0xAA, 0xAA, 0xAA];
    let weak_payload = vec![0xBB, 0xBB];
    sched.add_node(
        Box::new(OneShotSender {
            id: NodeId(1),
            tx: Some(make_tx(strong_payload.clone(), 7, 868_100_000, 50_000, 20)),
        }),
        Some(0),
    );
    sched.add_node(
        Box::new(OneShotSender {
            id: NodeId(2),
            tx: Some(make_tx(weak_payload.clone(), 7, 868_100_000, 50_000, 14)),
        }),
        Some(10_000),
    );
    sched.add_node(
        Box::new(RecordingReceiver {
            id: NodeId(3),
            received: Rc::clone(&received),
        }),
        None,
    );
    sched.run();

    let frames = received.borrow();
    // One delivery (the captured strong frame) to NodeId(2) (weak sender) and
    // NodeId(3) (passive receiver) — but Node 1 is also a sender so it never
    // receives. So 2 deliveries total, all carrying the strong sender's payload.
    assert_eq!(
        sched.metrics.total_captures, 1,
        "expected exactly one capture event"
    );
    assert!(
        !frames.is_empty(),
        "captured frame must reach at least one receiver"
    );
    for f in frames.iter() {
        assert_eq!(
            f.payload, strong_payload,
            "captured frame must carry the strong sender's payload, not the weak one"
        );
    }
}

// ---------------------------------------------------------------------------
// Invariant 5: interferer-originated frames preserve metadata
// ---------------------------------------------------------------------------

#[test]
fn interferer_originated_frame_matches_at_receiver() {
    let received = Rc::new(RefCell::new(Vec::new()));
    let mut sched = Scheduler::new(500_000);

    let inj_payload = vec![0xFF, 0xEE, 0xDD];
    let inj_tx = make_tx(inj_payload.clone(), 11, 868_500_000, 200_000, 17);

    sched.add_node(
        Box::new(RecordingReceiver {
            id: NodeId(1),
            received: Rc::clone(&received),
        }),
        None,
    );
    sched.add_interferer(
        Box::new(OneShotInterferer {
            tx: Some(inj_tx.clone()),
        }),
        0,
    );
    sched.run();

    let frames = received.borrow();
    assert_eq!(frames.len(), 1, "interferer TX must reach the receiver");
    let f = &frames[0];
    assert_eq!(f.payload, inj_payload);
    assert_eq!(f.sf, inj_tx.sf);
    assert_eq!(f.frequency, inj_tx.frequency);

    // RSSI/SNR derive from tx_power and channel config the same way for any
    // sender (node or interferer) — there is no "interferer discount".
    let cfg = ChannelConfig::lora_defaults();
    let expected_rssi = inj_tx.tx_power_dbm as f32 - cfg.path_loss_db;
    assert!((f.rssi - expected_rssi).abs() < 1e-3);
}
