use lorawan::default_crypto::DefaultFactory;
use lorawan::keys::AES128;
use lorawan::parser::{DataHeader, DataPayload, MHDRAble, MType, PhyPayload};
use lorawan_device::nb_device::radio::{Event, PhyRxTx, Response, RfConfig, RxQuality, TxConfig};
use lorawan_device::nb_device::{Device, Event as DevEvent};
use lorawan_device::{AppSKey, DevAddr, JoinMode, NewSKey, Timings};
use rand_core::{Error, RngCore, impls};

use theatron::types::Transmission;

struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 1 } else { seed })
    }
}

impl RngCore for Xorshift64 {
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        impls::fill_bytes_via_next(self, dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

#[derive(Debug)]
struct MinimalRadio {
    rx_buf: [u8; 256],
    rx_len: usize,
    pending_tx: Option<Transmission>,
    pending_downlink: Option<Vec<u8>>,
}

impl MinimalRadio {
    fn new() -> Self {
        Self {
            rx_buf: [0u8; 256],
            rx_len: 0,
            pending_tx: None,
            pending_downlink: None,
        }
    }

    fn take_pending_tx(&mut self) -> Option<Transmission> {
        self.pending_tx.take()
    }
}

impl PhyRxTx for MinimalRadio {
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
                    rf: RfConfig { frequency, bb, .. },
                    ..
                } = tx_config;
                let payload = buf.to_vec();
                let duration_us = bb.time_on_air_us(Some(8), true, payload.len() as u8) as u64;
                self.pending_tx = Some(Transmission {
                    payload,
                    sf: bb.sf.factor() as u8,
                    bandwidth: bb.bw.hz(),
                    coding_rate: bb.cr.denom() as u8,
                    frequency,
                    duration_us,
                    tx_power_dbm: tx_config.pw,
                });
                Ok(Response::TxDone(0))
            }
            Event::RxRequest(_) => Ok(Response::Rxing),
            Event::CancelRx => Ok(Response::Idle),
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

impl Timings for MinimalRadio {
    fn get_rx_window_offset_ms(&self) -> i32 {
        0
    }

    fn get_rx_window_duration_ms(&self) -> u32 {
        100
    }
}

const DEV_ID: u32 = 42;
const NWK_SKEY_BYTES: [u8; 16] = [DEV_ID as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
const APP_SKEY_BYTES: [u8; 16] = [DEV_ID as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

fn make_device() -> Device<MinimalRadio, Xorshift64, 255> {
    let radio = MinimalRadio::new();
    let rng = Xorshift64::new(0xDEAD_BEEF);
    let region = lorawan_device::region::Configuration::new(lorawan_device::Region::EU868);
    let mut device = Device::new(region, radio, rng);
    let credentials = JoinMode::ABP {
        devaddr: DevAddr::from(DEV_ID),
        appskey: AppSKey::from(APP_SKEY_BYTES),
        nwkskey: NewSKey::from(NWK_SKEY_BYTES),
    };
    device.join(credentials).expect("ABP join must succeed");
    device.set_datarate(lorawan_device::region::DR::_5);
    device
}

fn drain_rx_windows(device: &mut Device<MinimalRadio, Xorshift64, 255>) {
    for _ in 0..10 {
        if device.ready_to_send_data() {
            break;
        }
        let _ = device.handle_event(DevEvent::TimeoutFired);
    }
}

fn send_and_get_payload(
    device: &mut Device<MinimalRadio, Xorshift64, 255>,
    data: &[u8],
) -> Vec<u8> {
    drain_rx_windows(device);
    let result = device.send(data, 1, false);
    assert!(result.is_ok(), "send must succeed");
    let tx = device
        .get_radio()
        .take_pending_tx()
        .expect("radio must have pending TX after send");
    tx.payload
}

#[test]
fn uplink_frame_parses_as_valid_lorawan() {
    let mut device = make_device();
    let mut raw = send_and_get_payload(&mut device, &[0xAB, 0xCD]);

    let parsed = lorawan::parser::parse(raw.as_mut_slice()).expect("must parse as LoRaWAN frame");
    match parsed {
        PhyPayload::Data(DataPayload::Encrypted(ref phy)) => {
            assert_eq!(phy.mhdr().mtype(), MType::UnconfirmedDataUp);
            assert_eq!(phy.f_port(), Some(1));
        }
        _ => panic!("expected Data::Encrypted with UnconfirmedDataUp"),
    }
}

#[test]
fn uplink_mic_validates() {
    let mut device = make_device();
    let mut raw = send_and_get_payload(&mut device, &[0x01, 0x02, 0x03]);

    let parsed = lorawan::parser::parse(raw.as_mut_slice()).expect("must parse as LoRaWAN frame");
    if let PhyPayload::Data(DataPayload::Encrypted(ref phy)) = parsed {
        let nwk_skey = AES128(NWK_SKEY_BYTES);
        let fcnt = phy.fhdr().fcnt() as u32;
        assert!(
            phy.validate_mic(&nwk_skey, fcnt, &DefaultFactory),
            "MIC must validate with known NwkSKey"
        );
    } else {
        panic!("expected Data::Encrypted");
    }
}

#[test]
fn frame_counter_increments() {
    let mut device = make_device();

    let mut raw1 = send_and_get_payload(&mut device, &[0x01]);
    let mut raw2 = send_and_get_payload(&mut device, &[0x02]);

    let parsed1 = lorawan::parser::parse(raw1.as_mut_slice()).expect("frame 1 must parse");
    let parsed2 = lorawan::parser::parse(raw2.as_mut_slice()).expect("frame 2 must parse");

    let fcnt1 = if let PhyPayload::Data(DataPayload::Encrypted(ref phy)) = parsed1 {
        phy.fhdr().fcnt()
    } else {
        panic!("frame 1: expected Data::Encrypted");
    };

    let fcnt2 = if let PhyPayload::Data(DataPayload::Encrypted(ref phy)) = parsed2 {
        phy.fhdr().fcnt()
    } else {
        panic!("frame 2: expected Data::Encrypted");
    };

    assert_eq!(fcnt2, fcnt1 + 1, "frame counter must increment by 1");
}
