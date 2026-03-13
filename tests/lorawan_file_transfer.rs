use theatron::scheduler::{NodeHandle, Scheduler};
use theatron::time::{SimTime, ms_to_sim_time};
use theatron::traits::InterferenceSource;
use theatron::types::{ChannelEvent, NodeId, RxMetadata, Transmission};

fn make_tx_power(
    sf: u8,
    frequency: u32,
    duration_us: u64,
    payload: Vec<u8>,
    tx_power_dbm: i8,
) -> Transmission {
    Transmission {
        payload,
        sf,
        bandwidth: 125_000,
        coding_rate: 5,
        frequency,
        duration_us,
        tx_power_dbm,
    }
}

struct SequentialSender {
    id: NodeId,
    total: usize,
    sent: usize,
    pending: Option<Transmission>,
    gap_us: u64,
    sf: u8,
    frequency: u32,
    tx_duration_us: u64,
    tx_power_dbm: i8,
}

impl SequentialSender {
    fn new(
        id: u32,
        total: usize,
        gap_us: u64,
        sf: u8,
        frequency: u32,
        tx_duration_us: u64,
    ) -> Self {
        Self::with_power(id, total, gap_us, sf, frequency, tx_duration_us, 14)
    }

    fn with_power(
        id: u32,
        total: usize,
        gap_us: u64,
        sf: u8,
        frequency: u32,
        tx_duration_us: u64,
        tx_power_dbm: i8,
    ) -> Self {
        Self {
            id: NodeId(id),
            total,
            sent: 0,
            pending: None,
            gap_us,
            sf,
            frequency,
            tx_duration_us,
            tx_power_dbm,
        }
    }
}

impl NodeHandle for SequentialSender {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        None
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        self.pending.take()
    }

    fn update(&mut self, time: SimTime) -> Option<SimTime> {
        if self.sent >= self.total {
            return None;
        }
        self.pending = Some(make_tx_power(
            self.sf,
            self.frequency,
            self.tx_duration_us,
            vec![self.sent as u8],
            self.tx_power_dbm,
        ));
        self.sent += 1;
        Some(time + self.tx_duration_us + self.gap_us)
    }
}

struct SilentReceiver {
    id: NodeId,
    rx_count: usize,
}

impl SilentReceiver {
    fn new(id: u32) -> Self {
        Self {
            id: NodeId(id),
            rx_count: 0,
        }
    }
}

impl NodeHandle for SilentReceiver {
    fn node_id(&self) -> NodeId {
        self.id
    }

    fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
        self.rx_count += 1;
        None
    }

    fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
        None
    }

    fn update(&mut self, _time: SimTime) -> Option<SimTime> {
        None
    }
}

struct BurstInterferer {
    period_us: u64,
    duration_us: u64,
    sf: u8,
    frequency: u32,
    tx_power_dbm: i8,
}

impl InterferenceSource for BurstInterferer {
    fn observe(&mut self, _event: &ChannelEvent, _time: SimTime) {}

    fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
        Some(make_tx_power(
            self.sf,
            self.frequency,
            self.duration_us,
            vec![0xFF],
            self.tx_power_dbm,
        ))
    }

    fn next_poll_time(&self, current_time: SimTime) -> Option<SimTime> {
        Some(current_time + self.period_us)
    }
}

const TX_DURATION_US: u64 = 50_000;
const GAP_US: u64 = 200_000;
const SF: u8 = 7;
const FREQ: u32 = 868_100_000;

#[test]
fn all_fragments_delivered_without_interference() {
    const COUNT: usize = 10;
    let end = ms_to_sim_time(5_000);
    let mut scheduler = Scheduler::new(end);

    scheduler.add_node(
        Box::new(SequentialSender::new(
            1,
            COUNT,
            GAP_US,
            SF,
            FREQ,
            TX_DURATION_US,
        )),
        Some(0),
    );
    scheduler.add_node(Box::new(SilentReceiver::new(2)), None);
    scheduler.run();

    assert_eq!(scheduler.metrics.total_tx, COUNT as u64);
    assert_eq!(scheduler.metrics.total_rx, COUNT as u64);
    assert_eq!(scheduler.metrics.total_collisions, 0);
}

#[test]
fn interference_causes_collisions() {
    let end = ms_to_sim_time(5_000);
    let mut scheduler = Scheduler::new(end);

    scheduler.add_node(
        Box::new(SequentialSender::new(
            1,
            20,
            GAP_US,
            SF,
            FREQ,
            TX_DURATION_US,
        )),
        Some(0),
    );
    scheduler.add_node(Box::new(SilentReceiver::new(2)), None);

    scheduler.add_interferer(
        Box::new(BurstInterferer {
            period_us: 250_000,
            duration_us: 80_000,
            sf: SF,
            frequency: FREQ,
            tx_power_dbm: 14,
        }),
        25_000,
    );
    scheduler.run();

    assert!(
        scheduler.metrics.total_collisions > 0,
        "expected collisions"
    );
}

#[test]
fn deterministic_same_scenario_same_result() {
    fn run() -> (u64, u64, u64) {
        let end = ms_to_sim_time(2_000);
        let mut scheduler = Scheduler::new(end);
        scheduler.add_node(
            Box::new(SequentialSender::new(
                1,
                5,
                GAP_US,
                SF,
                FREQ,
                TX_DURATION_US,
            )),
            Some(0),
        );
        scheduler.add_node(Box::new(SilentReceiver::new(2)), None);
        scheduler.add_interferer(
            Box::new(BurstInterferer {
                period_us: 300_000,
                duration_us: 60_000,
                sf: SF,
                frequency: FREQ,
                tx_power_dbm: 14,
            }),
            100_000,
        );
        scheduler.run();
        (
            scheduler.metrics.total_tx,
            scheduler.metrics.total_rx,
            scheduler.metrics.total_collisions,
        )
    }

    assert_eq!(run(), run(), "simulation must be deterministic");
}

#[test]
fn pdr_greater_than_zero_under_interference() {
    let end = ms_to_sim_time(5_000);
    let mut scheduler = Scheduler::new(end);

    scheduler.add_node(
        Box::new(SequentialSender::new(
            1,
            20,
            GAP_US,
            SF,
            FREQ,
            TX_DURATION_US,
        )),
        Some(0),
    );
    scheduler.add_node(Box::new(SilentReceiver::new(2)), None);

    scheduler.add_interferer(
        Box::new(BurstInterferer {
            period_us: 500_000,
            duration_us: 60_000,
            sf: SF,
            frequency: FREQ,
            tx_power_dbm: 14,
        }),
        100_000,
    );
    scheduler.run();

    let tx = scheduler.metrics.total_tx;
    let rx = scheduler.metrics.total_rx;
    assert!(tx > 0);
    assert!(
        rx > 0,
        "some packets must be delivered even under interference"
    );
}

const SENDER_PERIOD_US: u64 = TX_DURATION_US + GAP_US;

fn sender_frames_at_receiver(m: &theatron::metrics::MetricsCollector) -> u64 {
    m.node_rx_count(NodeId(2))
        .saturating_sub(m.node_rx_count(NodeId(1)))
}

#[test]
fn strong_sender_survives_weak_interferer_via_capture() {
    let end = ms_to_sim_time(2_400);
    let mut scheduler = Scheduler::new(end);

    scheduler.add_node(
        Box::new(SequentialSender::with_power(
            1,
            10,
            GAP_US,
            SF,
            FREQ,
            TX_DURATION_US,
            20,
        )),
        Some(0),
    );
    scheduler.add_node(Box::new(SilentReceiver::new(2)), None);

    scheduler.add_interferer(
        Box::new(BurstInterferer {
            period_us: SENDER_PERIOD_US,
            duration_us: TX_DURATION_US,
            sf: SF,
            frequency: FREQ,
            tx_power_dbm: 14,
        }),
        0,
    );
    scheduler.run();

    assert_eq!(
        scheduler.metrics.total_tx, scheduler.metrics.total_rx,
        "all interferer TXs should be collided (captured by sender), so total_rx == total_tx"
    );
    assert!(
        scheduler.metrics.total_captures > 0,
        "capture events must be recorded"
    );
}

#[test]
fn strong_interferer_causes_more_collisions_than_weak() {
    let run_with_interferer_power = |power: i8| -> u64 {
        let end = ms_to_sim_time(4_900);
        let mut scheduler = Scheduler::new(end);
        scheduler.add_node(
            Box::new(SequentialSender::with_power(
                1,
                20,
                GAP_US,
                SF,
                FREQ,
                TX_DURATION_US,
                14,
            )),
            Some(0),
        );
        scheduler.add_node(Box::new(SilentReceiver::new(2)), None);
        scheduler.add_interferer(
            Box::new(BurstInterferer {
                period_us: SENDER_PERIOD_US,
                duration_us: TX_DURATION_US,
                sf: SF,
                frequency: FREQ,
                tx_power_dbm: power,
            }),
            0,
        );
        scheduler.run();
        sender_frames_at_receiver(&scheduler.metrics)
    };

    let delivered_with_weak = run_with_interferer_power(8);
    let delivered_with_strong = run_with_interferer_power(20);

    assert!(
        delivered_with_weak > delivered_with_strong,
        "weak interferer (capture effect) should allow more sender deliveries: weak={delivered_with_weak} strong={delivered_with_strong}"
    );
}
