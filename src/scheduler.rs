use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::channel::Channel;
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
    /// # Examples
    ///
    /// ```
    /// use theatron::scheduler::Scheduler;
    /// let sched = Scheduler::new(60_000_000);
    /// assert_eq!(sched.current_time(), 0);
    /// ```
    pub fn new(end_time: SimTime) -> Self {
        Self {
            events: BinaryHeap::new(),
            channel: Channel::new(),
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
        if let Some(wake) = initial_wake {
            let node_id = node.node_id();
            self.schedule(wake, EventKind::Wake { node_id });
        }
        self.nodes.push(node);
    }

    pub fn add_interferer(&mut self, interferer: Box<dyn InterferenceSource>, first_poll: SimTime) {
        let idx = self.interferers.len();
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
        let completed = self.channel.drain_completed();
        for (sender, collided, payload, sf, frequency, end_time) in completed {
            if collided {
                self.metrics.record_collision();
            } else {
                let frame = RxMetadata {
                    payload,
                    rssi: -80.0,
                    snr: 10.0,
                    sf,
                    frequency,
                    time: end_time,
                };
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
                EventKind::TxComplete { sender } => {
                    let completed_events = self.channel.resolve_at(event.time);
                    for ch_event in &completed_events {
                        for interferer in &mut self.interferers {
                            interferer.observe(ch_event, event.time);
                        }
                    }
                    self.deliver_completed_to_nodes(event.time);
                    let _ = sender;
                }
                EventKind::InterferencePoll { interferer_idx } => {
                    let time = event.time;
                    if let Some(tx) = self.interferers[interferer_idx].poll_inject(time) {
                        let duration = tx.duration_us;
                        let sf = tx.sf;
                        let frequency = tx.frequency;
                        let payload = tx.payload.clone();
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
                        let frame = RxMetadata {
                            payload,
                            rssi: -90.0,
                            snr: 5.0,
                            sf,
                            frequency,
                            time: complete_time,
                        };
                        let _ = frame;
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
    use crate::types::{ChannelEvent, Transmission};

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

    fn make_tx(sf: u8, frequency: u32, duration_us: u64) -> Transmission {
        Transmission {
            payload: vec![0xAB],
            sf,
            bandwidth: 125_000,
            coding_rate: 5,
            frequency,
            duration_us,
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
}
