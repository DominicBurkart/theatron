//! Tests for ALOHA-related behaviour.
//!
//! # Module 1 — `aloha_node` (integration tests for `AlohaNode`)
//!
//! Tests the `theatron::aloha::{AlohaNode, PoissonTraffic}` types directly,
//! covering single-node delivery, multi-node collision, and backoff retransmission.
//!
//! # Module 2 — `scheduler_patterns` (scheduler/channel model for ALOHA-like patterns)
//!
//! These tests use local `PeriodicSender`/`Receiver` helpers to validate the
//! scheduler + channel model for ALOHA-like transmission patterns (collision,
//! SF/frequency orthogonality, capture effect). They do not depend on `AlohaNode`.

// ===========================================================================
// Module 1: AlohaNode integration tests
// ===========================================================================

mod aloha_node {
    use std::cell::RefCell;
    use std::rc::Rc;

    use theatron::aloha::{AlohaNode, PoissonTraffic, LORA_SF7_DURATION_US};
    use theatron::scheduler::{NodeHandle, Scheduler};
    use theatron::time::SimTime;
    use theatron::traits::TrafficModel;
    use theatron::types::{NodeId, RxMetadata, Transmission};

    /// A traffic model that fires exactly once when `time >= fire_at`.
    struct FireOnce {
        payload: Vec<u8>,
        fire_at: SimTime,
        fired: bool,
    }

    impl FireOnce {
        fn new(payload: Vec<u8>, fire_at: SimTime) -> Self {
            Self {
                payload,
                fire_at,
                fired: false,
            }
        }
    }

    impl TrafficModel for FireOnce {
        fn next_payload(&mut self, time: SimTime) -> Option<Vec<u8>> {
            if !self.fired && time >= self.fire_at {
                self.fired = true;
                Some(self.payload.clone())
            } else {
                None
            }
        }
    }

    /// A pure receiver node that counts received frames.
    struct Receiver {
        id: NodeId,
    }

    impl Receiver {
        fn new(id: u32) -> Self {
            Self { id: NodeId(id) }
        }
    }

    impl NodeHandle for Receiver {
        fn node_id(&self) -> NodeId {
            self.id
        }
        fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
            None
        }
        fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
            None
        }
        fn update(&mut self, _time: SimTime) -> Option<SimTime> {
            None
        }
    }

    /// Traffic model that fires exactly `n` times, one payload per poll.
    struct NShot(u8);

    impl TrafficModel for NShot {
        fn next_payload(&mut self, _time: SimTime) -> Option<Vec<u8>> {
            if self.0 > 0 {
                self.0 -= 1;
                Some(vec![self.0])
            } else {
                None
            }
        }
    }

    /// Wraps an `AlohaNode` and records the simulation time of each TX.
    struct RecordingNode<T: TrafficModel> {
        inner: AlohaNode<T>,
        tx_times: Rc<RefCell<Vec<SimTime>>>,
    }

    impl<T: TrafficModel> NodeHandle for RecordingNode<T> {
        fn node_id(&self) -> NodeId {
            self.inner.node_id()
        }
        fn on_receive(&mut self, frame: RxMetadata, time: SimTime) -> Option<SimTime> {
            self.inner.on_receive(frame, time)
        }
        fn poll_transmit(&mut self, time: SimTime) -> Option<Transmission> {
            let tx = self.inner.poll_transmit(time);
            if tx.is_some() {
                self.tx_times.borrow_mut().push(time);
            }
            tx
        }
        fn update(&mut self, time: SimTime) -> Option<SimTime> {
            self.inner.update(time)
        }
    }

    #[test]
    fn single_node_delivery() {
        let sim_end = LORA_SF7_DURATION_US * 10;
        let mut scheduler = Scheduler::new(sim_end);

        let traffic = FireOnce::new(vec![0xAB], 0);
        let sender = AlohaNode::new(NodeId(1), traffic, 0, 1);
        scheduler.add_node(Box::new(sender), Some(0));
        scheduler.add_node(Box::new(Receiver::new(2)), None);

        scheduler.run();

        assert_eq!(scheduler.metrics.total_tx, 1, "exactly one TX expected");
        assert_eq!(
            scheduler.metrics.total_rx, 1,
            "single packet must be delivered to the receiver"
        );
        assert_eq!(
            scheduler.metrics.total_collisions, 0,
            "no collision with a single sender"
        );
    }

    #[test]
    fn multi_node_collision() {
        let sim_end = LORA_SF7_DURATION_US * 10;
        let mut scheduler = Scheduler::new(sim_end);

        scheduler.add_node(
            Box::new(AlohaNode::new(
                NodeId(1),
                FireOnce::new(vec![0x01], 0),
                0,
                10,
            )),
            Some(0),
        );
        scheduler.add_node(
            Box::new(AlohaNode::new(
                NodeId(2),
                FireOnce::new(vec![0x02], 0),
                0,
                20,
            )),
            Some(0),
        );
        scheduler.add_node(Box::new(Receiver::new(3)), None);

        scheduler.run();

        assert_eq!(scheduler.metrics.total_tx, 2, "both nodes must transmit");
        assert!(
            scheduler.metrics.total_collisions >= 2,
            "both concurrent same-SF/freq TXs must collide; got {}",
            scheduler.metrics.total_collisions
        );
        assert_eq!(
            scheduler.metrics.total_rx, 0,
            "no frame must be delivered when both collide"
        );
    }

    #[test]
    fn backoff_retransmission() {
        let backoff_range_us = 200_000u64;
        let sim_end = LORA_SF7_DURATION_US * 2 + backoff_range_us + 100_000;

        let tx_times: Rc<RefCell<Vec<SimTime>>> = Rc::new(RefCell::new(Vec::new()));
        let tx_times_out = Rc::clone(&tx_times);

        let mut scheduler = Scheduler::new(sim_end);
        scheduler.add_node(
            Box::new(RecordingNode {
                inner: AlohaNode::new(NodeId(1), NShot(2), backoff_range_us, 42),
                tx_times,
            }),
            Some(0),
        );
        scheduler.add_node(Box::new(Receiver::new(2)), None);
        scheduler.run();

        let times = tx_times_out.borrow();
        assert_eq!(
            times.len(),
            2,
            "two-shot traffic must produce exactly two TXs; got {}",
            times.len()
        );
        assert!(
            times[1] > times[0],
            "second TX ({}) must happen after first TX ({})",
            times[1],
            times[0]
        );
        assert!(
            times[1] >= times[0] + LORA_SF7_DURATION_US,
            "second TX must be at least one TX-duration after the first; \
             first={} second={} duration={}",
            times[0],
            times[1],
            LORA_SF7_DURATION_US
        );
        assert_eq!(scheduler.metrics.total_tx, 2);
    }
}

// ===========================================================================
// Module 2: Scheduler/channel model for ALOHA-like transmission patterns
// ===========================================================================

mod scheduler_patterns {
    use theatron::scheduler::{NodeHandle, Scheduler};
    use theatron::time::SimTime;
    use theatron::types::{NodeId, RxMetadata, Transmission};

    fn make_tx(payload: Vec<u8>, sf: u8, frequency: u32, duration_us: u64) -> Transmission {
        Transmission {
            payload,
            sf,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency,
            duration_us,
            tx_power_dbm: 14,
        }
    }

    fn make_tx_power(
        payload: Vec<u8>,
        sf: u8,
        frequency: u32,
        duration_us: u64,
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

    struct PeriodicSender {
        id: NodeId,
        interval_us: u64,
        duration_us: u64,
        remaining: usize,
        sf: u8,
        frequency: u32,
        pending: Option<Transmission>,
    }

    impl PeriodicSender {
        fn new(
            id: u32,
            interval_us: u64,
            duration_us: u64,
            count: usize,
            sf: u8,
            frequency: u32,
        ) -> Self {
            Self {
                id: NodeId(id),
                interval_us,
                duration_us,
                remaining: count,
                sf,
                frequency,
                pending: None,
            }
        }
    }

    impl NodeHandle for PeriodicSender {
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
            if self.remaining > 0 {
                self.remaining -= 1;
                self.pending = Some(make_tx(
                    vec![self.id.0 as u8; 10],
                    self.sf,
                    self.frequency,
                    self.duration_us,
                ));
                Some(time + self.interval_us)
            } else {
                None
            }
        }
    }

    struct PoweredSender {
        id: NodeId,
        sf: u8,
        frequency: u32,
        duration_us: u64,
        tx_power_dbm: i8,
        fired: bool,
    }

    impl PoweredSender {
        fn new(id: u32, sf: u8, frequency: u32, duration_us: u64, tx_power_dbm: i8) -> Self {
            Self {
                id: NodeId(id),
                sf,
                frequency,
                duration_us,
                tx_power_dbm,
                fired: false,
            }
        }
    }

    impl NodeHandle for PoweredSender {
        fn node_id(&self) -> NodeId {
            self.id
        }
        fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
            None
        }
        fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
            if !self.fired {
                self.fired = true;
                Some(make_tx_power(
                    vec![self.id.0 as u8; 4],
                    self.sf,
                    self.frequency,
                    self.duration_us,
                    self.tx_power_dbm,
                ))
            } else {
                None
            }
        }
        fn update(&mut self, _time: SimTime) -> Option<SimTime> {
            None
        }
    }

    struct Receiver {
        id: NodeId,
        count: usize,
    }

    impl Receiver {
        fn new(id: u32) -> Self {
            Self {
                id: NodeId(id),
                count: 0,
            }
        }
    }

    impl NodeHandle for Receiver {
        fn node_id(&self) -> NodeId {
            self.id
        }
        fn on_receive(&mut self, _frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
            self.count += 1;
            None
        }
        fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
            None
        }
        fn update(&mut self, _time: SimTime) -> Option<SimTime> {
            None
        }
    }

    #[test]
    fn single_sender_all_delivered() {
        let mut sched = Scheduler::new(20_000_000);
        let sender = PeriodicSender::new(1, 1_000_000, 50_000, 5, 7, 868_100_000);
        let receiver = Receiver::new(99);
        sched.add_node(Box::new(sender), Some(0));
        sched.add_node(Box::new(receiver), None);
        sched.run();
        assert_eq!(sched.metrics.total_tx, 5);
        assert_eq!(sched.metrics.total_rx, 5);
        assert_eq!(sched.metrics.total_collisions, 0);
    }

    #[test]
    fn two_simultaneous_senders_collide() {
        let mut sched = Scheduler::new(1_000_000);
        let sender1 = PeriodicSender::new(1, 500_000, 200_000, 1, 7, 868_100_000);
        let sender2 = PeriodicSender::new(2, 500_000, 200_000, 1, 7, 868_100_000);
        let receiver = Receiver::new(99);
        sched.add_node(Box::new(sender1), Some(0));
        sched.add_node(Box::new(sender2), Some(0));
        sched.add_node(Box::new(receiver), None);
        sched.run();
        assert_eq!(sched.metrics.total_tx, 2);
        assert!(
            sched.metrics.total_collisions >= 1,
            "simultaneous same-SF/freq transmissions should collide"
        );
    }

    #[test]
    fn different_frequencies_no_collision() {
        let mut sched = Scheduler::new(1_000_000);
        let sender1 = PeriodicSender::new(1, 500_000, 200_000, 1, 7, 868_100_000);
        let sender2 = PeriodicSender::new(2, 500_000, 200_000, 1, 7, 868_300_000);
        let receiver = Receiver::new(99);
        sched.add_node(Box::new(sender1), Some(0));
        sched.add_node(Box::new(sender2), Some(0));
        sched.add_node(Box::new(receiver), None);
        sched.run();
        assert_eq!(sched.metrics.total_tx, 2);
        assert_eq!(sched.metrics.total_rx, 4);
        assert_eq!(sched.metrics.total_collisions, 0);
    }

    #[test]
    fn different_sf_no_collision() {
        let mut sched = Scheduler::new(1_000_000);
        let sender1 = PeriodicSender::new(1, 500_000, 200_000, 1, 7, 868_100_000);
        let sender2 = PeriodicSender::new(2, 500_000, 200_000, 1, 12, 868_100_000);
        let receiver = Receiver::new(99);
        sched.add_node(Box::new(sender1), Some(0));
        sched.add_node(Box::new(sender2), Some(0));
        sched.add_node(Box::new(receiver), None);
        sched.run();
        assert_eq!(sched.metrics.total_tx, 2);
        assert_eq!(sched.metrics.total_rx, 4);
        assert_eq!(sched.metrics.total_collisions, 0);
    }

    #[test]
    fn sequential_transmissions_no_collision() {
        let mut sched = Scheduler::new(20_000_000);
        let sender1 = PeriodicSender::new(1, 2_000_000, 200_000, 3, 7, 868_100_000);
        let sender2 = PeriodicSender::new(2, 2_000_000, 200_000, 3, 7, 868_100_000);
        let receiver = Receiver::new(99);
        sched.add_node(Box::new(sender1), Some(0));
        sched.add_node(Box::new(sender2), Some(1_000_000));
        sched.add_node(Box::new(receiver), None);
        sched.run();
        assert_eq!(sched.metrics.total_tx, 6);
        assert_eq!(sched.metrics.total_collisions, 0);
    }

    #[test]
    fn five_simultaneous_senders_high_collision_rate() {
        let mut sched = Scheduler::new(1_000_000);
        for i in 1..=5u32 {
            let sender = PeriodicSender::new(i, 500_000, 200_000, 1, 7, 868_100_000);
            sched.add_node(Box::new(sender), Some(0));
        }
        let receiver = Receiver::new(99);
        sched.add_node(Box::new(receiver), None);
        sched.run();
        assert_eq!(sched.metrics.total_tx, 5);
        assert!(
            sched.metrics.total_collisions >= 1,
            "5 simultaneous senders must produce collisions"
        );
        assert_eq!(sched.metrics.total_rx, 0);
    }

    #[test]
    fn capture_effect_recorded_in_metrics() {
        let mut sched = Scheduler::new(1_000_000);
        let strong = PoweredSender::new(1, 7, 868_100_000, 200_000, 20);
        let weak = PoweredSender::new(2, 7, 868_100_000, 200_000, 14);
        let receiver = Receiver::new(99);
        sched.add_node(Box::new(strong), Some(0));
        sched.add_node(Box::new(weak), Some(0));
        sched.add_node(Box::new(receiver), None);
        sched.run();
        assert_eq!(sched.metrics.total_tx, 2);
        assert_eq!(sched.metrics.total_captures, 1, "expected one capture event");
        assert_eq!(
            sched.metrics.total_collisions, 1,
            "weak sender should be marked collided"
        );
        assert_eq!(
            sched.metrics.total_rx, 2,
            "captured frame delivered to 2 non-sender nodes (weak sender + receiver)"
        );
    }
}
