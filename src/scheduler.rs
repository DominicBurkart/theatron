use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::channel::{Channel, ChannelConfig, CompletedTx};
use crate::metrics::MetricsCollector;
use crate::time::SimTime;
use crate::traits::InterferenceSource;
use crate::types::{NodeId, RxMetadata, Transmission};

/// A handle to a simulation node, allowing the scheduler to drive it.
///
/// # Examples
///
/// ```
/// use theatron::scheduler::{NodeHandle, Scheduler};
/// use theatron::time::SimTime;
/// use theatron::types::{NodeId, RxMetadata, Transmission};
///
/// struct Ping { id: NodeId }
///
/// impl NodeHandle for Ping {
///     fn node_id(&self) -> NodeId { self.id }
///     fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> { None }
///     fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> { None }
///     fn update(&mut self, _t: SimTime) -> Option<SimTime> { None }
/// }
///
/// let mut sched = Scheduler::new(1_000_000);
/// sched.add_node(Box::new(Ping { id: NodeId(1) }), None);
/// sched.run();
/// ```
pub trait NodeHandle {
    fn node_id(&self) -> NodeId;
    fn on_receive(&mut self, frame: RxMetadata, time: SimTime) -> Option<SimTime>;
    fn poll_transmit(&mut self, time: SimTime) -> Option<Transmission>;
    fn update(&mut self, time: SimTime) -> Option<SimTime>;
}

/// The kind of event processed by the scheduler.
///
/// # Examples
///
/// ```
/// use theatron::scheduler::EventKind;
/// use theatron::types::NodeId;
///
/// let wake = EventKind::Wake { node_id: NodeId(1) };
/// match wake {
///     EventKind::Wake { node_id } => assert_eq!(node_id, NodeId(1)),
///     _ => panic!("expected Wake"),
/// }
/// ```
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum EventKind {
    Wake { node_id: NodeId },
    TxComplete { sender: NodeId },
    InterferencePoll { interferer_idx: usize },
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ScheduledEvent {
    time: SimTime,
    seq: u64,
    kind: EventKind,
}

impl Ord for ScheduledEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        other.time.cmp(&self.time).then(other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for ScheduledEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The simulation scheduler, which drives all nodes and interferers.
pub struct Scheduler {
    events: BinaryHeap<ScheduledEvent>,
    channel: Channel,
    nodes: Vec<Box<dyn NodeHandle>>,
    interferers: Vec<Box<dyn InterferenceSource>>,
    pub metrics: MetricsCollector,
    current_time: SimTime,
    seq: u64,
    end_time: SimTime,
}

impl Scheduler {
    /// Create a new scheduler that will stop at `end_time` microseconds.
    ///
    /// Uses LoRa default channel parameters. To use a different protocol's
    /// physical-layer parameters, see [`Scheduler::with_channel`].
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::scheduler::Scheduler;
    /// let sched = Scheduler::new(60_000_000);
    /// assert_eq!(sched.current_time(), 0);
    /// ```
    pub fn new(end_time: SimTime) -> Self {
        Self::with_channel(end_time, Channel::new())
    }

    /// Create a new scheduler with a custom [`Channel`].
    ///
    /// Use this to simulate protocols other than LoRa by supplying a
    /// [`Channel`] constructed with [`Channel::with_config`].
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::channel::{Channel, ChannelConfig};
    /// use theatron::scheduler::Scheduler;
    ///
    /// // 802.15.4-like parameters
    /// let channel = Channel::with_config(ChannelConfig {
    ///     path_loss_db: 80.0,
    ///     noise_floor_dbm: -100.0,
    ///     co_channel_rejection_db: 3.0,
    /// });
    /// let sched = Scheduler::with_channel(1_000_000, channel);
    /// assert_eq!(sched.current_time(), 0);
    /// ```
    pub fn with_channel(end_time: SimTime, channel: Channel) -> Self {
        Self {
            events: BinaryHeap::new(),
            channel,
            nodes: Vec::new(),
            interferers: Vec::new(),
            metrics: MetricsCollector::new(),
            current_time: 0,
            seq: 0,
            end_time,
        }
    }

    /// Register a node with an optional initial wake time.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::scheduler::{NodeHandle, Scheduler};
    /// use theatron::time::SimTime;
    /// use theatron::types::{NodeId, RxMetadata, Transmission};
    ///
    /// struct Silent { id: NodeId }
    /// impl NodeHandle for Silent {
    ///     fn node_id(&self) -> NodeId { self.id }
    ///     fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> { None }
    ///     fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> { None }
    ///     fn update(&mut self, _t: SimTime) -> Option<SimTime> { None }
    /// }
    ///
    /// let mut sched = Scheduler::new(1_000_000);
    /// sched.add_node(Box::new(Silent { id: NodeId(1) }), None);
    /// sched.run();
    /// assert_eq!(sched.metrics.total_tx, 0);
    /// ```
    pub fn add_node(&mut self, node: Box<dyn NodeHandle>, initial_wake: Option<SimTime>) {
        debug_assert!(
            (0..self.interferers.len()).all(|i| node.node_id().0 != u32::MAX - i as u32),
            "NodeId({}) collides with interferer ID space",
            node.node_id().0
        );
        if let Some(wake) = initial_wake {
            let node_id = node.node_id();
            self.schedule(wake, EventKind::Wake { node_id });
        }
        self.nodes.push(node);
    }

    pub fn add_interferer(&mut self, interferer: Box<dyn InterferenceSource>, first_poll: SimTime) {
        let idx = self.interferers.len();
        let synthetic_id = u32::MAX - idx as u32;
        debug_assert!(
            self.nodes.iter().all(|n| n.node_id().0 != synthetic_id),
            "interferer synthetic NodeId({synthetic_id}) collides with a registered node"
        );
        self.interferers.push(interferer);
        self.schedule(
            first_poll,
            EventKind::InterferencePoll {
                interferer_idx: idx,
            },
        );
    }

    fn schedule(&mut self, time: SimTime, kind: EventKind) {
        let seq = self.seq;
        self.seq += 1;
        self.events.push(ScheduledEvent { time, seq, kind });
    }

    fn find_node_idx(&self, id: NodeId) -> Option<usize> {
        self.nodes.iter().position(|n| n.node_id() == id)
    }

    fn handle_poll_transmit(&mut self, node_idx: usize, time: SimTime) {
        if let Some(tx) = self.nodes[node_idx].poll_transmit(time) {
            let sender = self.nodes[node_idx].node_id();
            let duration = tx.duration_us;
            let ch_event = self.channel.begin_transmission(sender, &tx, time);
            for interferer in &mut self.interferers {
                interferer.observe(&ch_event, time);
            }
            self.metrics.record_tx(sender);
            self.metrics.record_airtime(duration);
            let complete_time = time + duration;
            self.schedule(complete_time, EventKind::TxComplete { sender });
        }
    }

    fn deliver_completed_to_nodes(&mut self, time: SimTime) {
        let completed: Vec<CompletedTx> = self.channel.drain_completed();
        for (sender, collided, captured, frame) in completed {
            if collided {
                self.metrics.record_collision();
            } else {
                if captured {
                    self.metrics.record_capture();
                }
                let mut wakes = Vec::new();
                for i in 0..self.nodes.len() {
                    if self.nodes[i].node_id() != sender {
                        let next = self.nodes[i].on_receive(frame.clone(), time);
                        self.metrics.record_rx(self.nodes[i].node_id());
                        if let Some(t) = next {
                            wakes.push((self.nodes[i].node_id(), t));
                        }
                    }
                }
                for (node_id, t) in wakes {
                    self.schedule(t, EventKind::Wake { node_id });
                }
                let mut tx_node_idxs = Vec::new();
                for i in 0..self.nodes.len() {
                    if self.nodes[i].node_id() != sender {
                        tx_node_idxs.push(i);
                    }
                }
                for i in tx_node_idxs {
                    self.handle_poll_transmit(i, time);
                }
            }
        }
    }

    /// Run the simulation until `end_time` or until there are no more events.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::scheduler::{NodeHandle, Scheduler};
    /// use theatron::time::SimTime;
    /// use theatron::types::{NodeId, RxMetadata, Transmission};
    ///
    /// struct Noop { id: NodeId }
    /// impl NodeHandle for Noop {
    ///     fn node_id(&self) -> NodeId { self.id }
    ///     fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> { None }
    ///     fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> { None }
    ///     fn update(&mut self, _t: SimTime) -> Option<SimTime> { None }
    /// }
    ///
    /// let mut sched = Scheduler::new(1_000_000);
    /// sched.add_node(Box::new(Noop { id: NodeId(1) }), Some(0));
    /// sched.run();
    /// assert!(sched.current_time() <= 1_000_000);
    /// ```
    pub fn run(&mut self) {
        while let Some(event) = self.events.pop() {
            if event.time > self.end_time {
                break;
            }
            self.current_time = event.time;

            match event.kind {
                EventKind::Wake { node_id } => {
                    if let Some(idx) = self.find_node_idx(node_id) {
                        let next = self.nodes[idx].update(event.time);
                        if let Some(t) = next {
                            self.schedule(t, EventKind::Wake { node_id });
                        }
                        self.handle_poll_transmit(idx, event.time);
                    }
                }
                EventKind::TxComplete { sender: _ } => {
                    let completed_events = self.channel.resolve_at(event.time);
                    for ch_event in &completed_events {
                        for interferer in &mut self.interferers {
                            interferer.observe(ch_event, event.time);
                        }
                    }
                    self.deliver_completed_to_nodes(event.time);
                }
                EventKind::InterferencePoll { interferer_idx } => {
                    let time = event.time;
                    if let Some(tx) = self.interferers[interferer_idx].poll_inject(time) {
                        let duration = tx.duration_us;
                        let interferer_node_id = NodeId(u32::MAX - interferer_idx as u32);
                        let ch_event =
                            self.channel
                                .begin_transmission(interferer_node_id, &tx, time);
                        for i in 0..self.interferers.len() {
                            self.interferers[i].observe(&ch_event, time);
                        }
                        self.metrics.record_airtime(duration);
                        let complete_time = time + duration;
                        self.schedule(
                            complete_time,
                            EventKind::TxComplete {
                                sender: interferer_node_id,
                            },
                        );
                    }
                    let next = self.interferers[interferer_idx].next_poll_time(time);
                    if let Some(t) = next {
                        self.schedule(t, EventKind::InterferencePoll { interferer_idx });
                    }
                }
            }
        }
    }

    /// Return the current simulation time in microseconds.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::scheduler::Scheduler;
    /// let sched = Scheduler::new(1_000_000);
    /// assert_eq!(sched.current_time(), 0);
    /// ```
    pub fn current_time(&self) -> SimTime {
        self.current_time
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::ChannelConfig;
    use crate::types::{ChannelEvent, Transmission};
    use proptest::prelude::*;

    struct SimpleNode {
        id: NodeId,
        pending_tx: Option<Transmission>,
        received: Vec<RxMetadata>,
        wake_at: Option<SimTime>,
    }

    impl SimpleNode {
        fn new(id: u32) -> Self {
            Self {
                id: NodeId(id),
                pending_tx: None,
                received: Vec::new(),
                wake_at: None,
            }
        }

        fn queue_tx(&mut self, tx: Transmission) {
            self.pending_tx = Some(tx);
        }
    }

    impl NodeHandle for SimpleNode {
        fn node_id(&self) -> NodeId {
            self.id
        }

        fn on_receive(&mut self, frame: RxMetadata, _time: SimTime) -> Option<SimTime> {
            self.received.push(frame);
            None
        }

        fn poll_transmit(&mut self, _time: SimTime) -> Option<Transmission> {
            self.pending_tx.take()
        }

        fn update(&mut self, _time: SimTime) -> Option<SimTime> {
            self.wake_at.take()
        }
    }

    struct PeriodicNode {
        id: NodeId,
        period: SimTime,
        wake_count: u32,
    }

    impl PeriodicNode {
        fn new(id: u32, period: SimTime) -> Self {
            Self {
                id: NodeId(id),
                period,
                wake_count: 0,
            }
        }
    }

    impl NodeHandle for PeriodicNode {
        fn node_id(&self) -> NodeId {
            self.id
        }

        fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> {
            None
        }

        fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> {
            None
        }

        fn update(&mut self, time: SimTime) -> Option<SimTime> {
            self.wake_count += 1;
            Some(time + self.period)
        }
    }

    struct WakeOnReceive {
        id: NodeId,
        wake_delay_us: u64,
    }

    impl NodeHandle for WakeOnReceive {
        fn node_id(&self) -> NodeId {
            self.id
        }

        fn on_receive(&mut self, _f: RxMetadata, time: SimTime) -> Option<SimTime> {
            Some(time + self.wake_delay_us)
        }

        fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> {
            None
        }

        fn update(&mut self, _t: SimTime) -> Option<SimTime> {
            None
        }
    }

    struct NoOpInterferer;

    impl InterferenceSource for NoOpInterferer {
        fn observe(&mut self, _event: &ChannelEvent, _time: SimTime) {}
        fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
            None
        }
        fn next_poll_time(&self, _current_time: SimTime) -> Option<SimTime> {
            None
        }
    }

    struct ActiveInterferer {
        tx: Transmission,
        poll_interval: u64,
        remaining: usize,
    }

    impl InterferenceSource for ActiveInterferer {
        fn observe(&mut self, _event: &ChannelEvent, _time: SimTime) {}
        fn poll_inject(&mut self, _time: SimTime) -> Option<Transmission> {
            if self.remaining > 0 {
                self.remaining -= 1;
                Some(self.tx.clone())
            } else {
                None
            }
        }
        fn next_poll_time(&self, current_time: SimTime) -> Option<SimTime> {
            if self.remaining > 0 {
                Some(current_time + self.poll_interval)
            } else {
                None
            }
        }
    }

    struct ReplyNode {
        id: NodeId,
        reply_tx: Option<Transmission>,
        received: bool,
    }

    impl NodeHandle for ReplyNode {
        fn node_id(&self) -> NodeId {
            self.id
        }
        fn on_receive(&mut self, _f: RxMetadata, _t: SimTime) -> Option<SimTime> {
            self.received = true;
            None
        }
        fn poll_transmit(&mut self, _t: SimTime) -> Option<Transmission> {
            if self.received {
                self.reply_tx.take()
            } else {
                None
            }
        }
        fn update(&mut self, _t: SimTime) -> Option<SimTime> {
            None
        }
    }

    fn make_tx(sf: u8, frequency: u32, duration_us: u64) -> Transmission {
        Transmission {
            payload: vec![0xAB],
            sf,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency,
            duration_us,
            tx_power_dbm: 14,
        }
    }

    #[test]
    fn single_node_tx_is_counted() {
        let mut scheduler = Scheduler::new(100_000);
        let mut node = SimpleNode::new(1);
        node.queue_tx(make_tx(7, 868_100_000, 50_000));
        scheduler.add_node(Box::new(node), Some(0));
        scheduler.run();
        assert_eq!(scheduler.metrics.total_tx, 1);
    }

    #[test]
    fn two_nodes_deliver_to_each_other() {
        let mut scheduler = Scheduler::new(200_000);
        let mut sender = SimpleNode::new(1);
        sender.queue_tx(make_tx(7, 868_100_000, 50_000));
        let receiver = SimpleNode::new(2);
        scheduler.add_node(Box::new(sender), Some(0));
        scheduler.add_node(Box::new(receiver), None);
        scheduler.run();
        assert_eq!(scheduler.metrics.total_tx, 1);
        assert_eq!(scheduler.metrics.total_rx, 1);
    }

    #[test]
    fn rx_returning_wake_schedules_node() {
        let mut scheduler = Scheduler::new(200_000);
        let mut sender = SimpleNode::new(1);
        sender.queue_tx(make_tx(7, 868_100_000, 50_000));
        scheduler.add_node(Box::new(sender), Some(0));
        scheduler.add_node(
            Box::new(WakeOnReceive {
                id: NodeId(2),
                wake_delay_us: 10_000,
            }),
            None,
        );
        scheduler.run();
        assert_eq!(scheduler.metrics.total_rx, 1);
    }

    #[test]
    fn interferer_registration_does_not_panic() {
        let mut scheduler = Scheduler::new(100_000);
        scheduler.add_interferer(Box::new(NoOpInterferer), 0);
        scheduler.run();
    }

    #[test]
    fn simulation_stops_at_end_time() {
        let end_time = 1_000_000u64;
        let mut scheduler = Scheduler::new(end_time);
        scheduler.add_node(Box::new(PeriodicNode::new(1, 100_000)), Some(0));
        scheduler.run();
        assert!(scheduler.current_time() <= end_time);
    }

    #[test]
    fn add_node_without_wake_never_wakes() {
        let mut scheduler = Scheduler::new(100_000);
        scheduler.add_node(Box::new(PeriodicNode::new(1, 10_000)), None);
        scheduler.run();
        assert_eq!(scheduler.current_time(), 0);
        assert_eq!(scheduler.metrics.total_tx, 0);
    }

    #[test]
    fn add_node_after_interferer_validates_id_space() {
        let mut sched = Scheduler::new(200_000);
        sched.add_interferer(Box::new(NoOpInterferer), 0);
        // Adding a node after an interferer exercises the debug_assert!
        // closure that checks for NodeId/interferer ID space collisions.
        sched.add_node(Box::new(SimpleNode::new(1)), Some(0));
        sched.run();
        assert_eq!(sched.metrics.total_tx, 0);
    }

    #[test]
    fn two_nodes_overlapping_tx_records_collision() {
        let mut sched = Scheduler::new(200_000);
        let mut n1 = SimpleNode::new(1);
        n1.queue_tx(make_tx(7, 868_100_000, 50_000));
        let mut n2 = SimpleNode::new(2);
        n2.queue_tx(make_tx(7, 868_100_000, 50_000));
        sched.add_node(Box::new(n1), Some(0));
        sched.add_node(Box::new(n2), Some(10_000));
        sched.run();

        assert_eq!(sched.metrics.total_tx, 2);
        assert_eq!(
            sched.metrics.total_collisions, 2,
            "overlapping same-SF/freq TXs must both collide"
        );
        assert_eq!(sched.metrics.total_rx, 0);
    }

    #[test]
    fn active_interferer_injects_and_records_airtime() {
        let mut sched = Scheduler::new(300_000);
        let interferer = ActiveInterferer {
            tx: make_tx(7, 868_100_000, 30_000),
            poll_interval: 100_000,
            remaining: 2,
        };
        sched.add_interferer(Box::new(interferer), 0);
        sched.run();

        assert_eq!(sched.metrics.total_airtime_us, 60_000);
        assert_eq!(
            sched.metrics.total_tx, 0,
            "interferer TXs do not count as node TXs"
        );
    }

    #[test]
    fn single_tx_airtime_is_recorded() {
        let mut sched = Scheduler::new(200_000);
        let mut node = SimpleNode::new(1);
        node.queue_tx(make_tx(7, 868_100_000, 75_000));
        sched.add_node(Box::new(node), Some(0));
        sched.run();

        assert_eq!(sched.metrics.total_airtime_us, 75_000);
    }

    #[test]
    fn receive_triggers_reply_tx() {
        let mut sched = Scheduler::new(200_000);
        let mut sender = SimpleNode::new(1);
        sender.queue_tx(make_tx(7, 868_100_000, 50_000));
        sched.add_node(Box::new(sender), Some(0));

        let reply_node = ReplyNode {
            id: NodeId(2),
            reply_tx: Some(make_tx(7, 868_100_000, 30_000)),
            received: false,
        };
        sched.add_node(Box::new(reply_node), None);
        sched.run();

        assert_eq!(sched.metrics.total_tx, 2, "original + reply");
        assert_eq!(sched.metrics.total_airtime_us, 80_000);
        assert_eq!(
            sched.metrics.total_rx, 2,
            "each node receives the other's TX"
        );
    }

    #[test]
    fn broadcast_to_three_receivers() {
        let mut sched = Scheduler::new(200_000);
        let mut sender = SimpleNode::new(1);
        sender.queue_tx(make_tx(7, 868_100_000, 50_000));
        sched.add_node(Box::new(sender), Some(0));
        sched.add_node(Box::new(SimpleNode::new(2)), None);
        sched.add_node(Box::new(SimpleNode::new(3)), None);
        sched.add_node(Box::new(SimpleNode::new(4)), None);
        sched.run();

        assert_eq!(sched.metrics.total_tx, 1);
        assert_eq!(sched.metrics.total_rx, 3);
        assert_eq!(sched.metrics.node_rx_count(NodeId(2)), 1);
        assert_eq!(sched.metrics.node_rx_count(NodeId(3)), 1);
        assert_eq!(sched.metrics.node_rx_count(NodeId(4)), 1);
        assert_eq!(sched.metrics.node_rx_count(NodeId(1)), 0);
    }

    #[test]
    fn capture_effect_through_scheduler() {
        let mut sched = Scheduler::new(200_000);
        let mut strong = SimpleNode::new(1);
        strong.queue_tx(Transmission {
            payload: vec![0xAB],
            sf: 7,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency: 868_100_000,
            duration_us: 50_000,
            tx_power_dbm: 20,
        });
        let mut weak = SimpleNode::new(2);
        weak.queue_tx(Transmission {
            payload: vec![0xCD],
            sf: 7,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency: 868_100_000,
            duration_us: 50_000,
            tx_power_dbm: 14,
        });
        sched.add_node(Box::new(strong), Some(0));
        sched.add_node(Box::new(weak), Some(10_000));
        sched.add_node(Box::new(SimpleNode::new(3)), None);
        sched.run();

        assert_eq!(sched.metrics.total_tx, 2);
        assert_eq!(sched.metrics.total_captures, 1);
        assert_eq!(sched.metrics.total_collisions, 1);
        assert_eq!(
            sched.metrics.total_rx, 2,
            "strong TX delivered to both non-sender nodes"
        );
    }

    #[test]
    fn interferer_collides_with_node_tx() {
        let mut sched = Scheduler::new(200_000);
        let mut node = SimpleNode::new(1);
        node.queue_tx(make_tx(7, 868_100_000, 50_000));
        sched.add_node(Box::new(node), Some(0));
        sched.add_node(Box::new(SimpleNode::new(2)), None);

        let interferer = ActiveInterferer {
            tx: make_tx(7, 868_100_000, 50_000),
            poll_interval: 0,
            remaining: 1,
        };
        sched.add_interferer(Box::new(interferer), 10_000);
        sched.run();

        assert!(sched.metrics.total_collisions > 0);
        assert_eq!(sched.metrics.total_rx, 0, "collision prevents delivery");
    }

    /// Verify that `Scheduler::with_channel` propagates custom physical-layer
    /// parameters: a strict capture threshold (10 dB) means a 6 dB delta that
    /// would survive on the default LoRa channel instead collides.
    #[test]
    fn with_channel_uses_custom_config() {
        let strict_channel = Channel::with_config(ChannelConfig {
            path_loss_db: 100.0,
            noise_floor_dbm: -117.0,
            co_channel_rejection_db: 10.0, // stricter than LoRa default of 6 dB
        });

        // With the default LoRa channel (threshold=6), delta=6 → strong survives.
        let mut sched_lora = Scheduler::new(200_000);
        let mut strong_lora = SimpleNode::new(1);
        strong_lora.queue_tx(Transmission {
            payload: vec![0xAB],
            sf: 7,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency: 868_100_000,
            duration_us: 50_000,
            tx_power_dbm: 20,
        });
        let mut weak_lora = SimpleNode::new(2);
        weak_lora.queue_tx(Transmission {
            payload: vec![0xCD],
            sf: 7,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency: 868_100_000,
            duration_us: 50_000,
            tx_power_dbm: 14,
        });
        sched_lora.add_node(Box::new(strong_lora), Some(0));
        sched_lora.add_node(Box::new(weak_lora), Some(10_000));
        sched_lora.add_node(Box::new(SimpleNode::new(3)), None);
        sched_lora.run();

        // With the strict channel (threshold=10), delta=6 → both collide.
        let mut sched_strict = Scheduler::with_channel(200_000, strict_channel);
        let mut strong_strict = SimpleNode::new(1);
        strong_strict.queue_tx(Transmission {
            payload: vec![0xAB],
            sf: 7,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency: 868_100_000,
            duration_us: 50_000,
            tx_power_dbm: 20,
        });
        let mut weak_strict = SimpleNode::new(2);
        weak_strict.queue_tx(Transmission {
            payload: vec![0xCD],
            sf: 7,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency: 868_100_000,
            duration_us: 50_000,
            tx_power_dbm: 14,
        });
        sched_strict.add_node(Box::new(strong_strict), Some(0));
        sched_strict.add_node(Box::new(weak_strict), Some(10_000));
        sched_strict.add_node(Box::new(SimpleNode::new(3)), None);
        sched_strict.run();

        assert_eq!(
            sched_lora.metrics.total_rx, 2,
            "LoRa channel: strong signal captured, delivered to both other nodes"
        );
        assert_eq!(
            sched_strict.metrics.total_rx, 0,
            "strict channel: delta=6 < threshold=10, both collide, nothing delivered"
        );
    }

    proptest! {
        #[test]
        fn n_receivers_all_get_broadcast(n in 2usize..20) {
            let mut sched = Scheduler::new(200_000);
            let mut sender = SimpleNode::new(0);
            sender.queue_tx(make_tx(7, 868_100_000, 50_000));
            sched.add_node(Box::new(sender), Some(0));
            for i in 1..=n {
                sched.add_node(Box::new(SimpleNode::new(i as u32)), None);
            }
            sched.run();
            prop_assert_eq!(sched.metrics.total_tx, 1u64);
            prop_assert_eq!(sched.metrics.total_rx, n as u64);
        }
    }
}
