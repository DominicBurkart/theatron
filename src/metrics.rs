use crate::types::NodeId;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct MetricsCollector {
    pub total_tx: u64,
    pub total_rx: u64,
    pub total_collisions: u64,
    pub total_airtime_us: u64,
    per_node_tx: HashMap<NodeId, u64>,
    per_node_rx: HashMap<NodeId, u64>,
}

impl MetricsCollector {
    /// Create a new metrics collector with all counters zeroed.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::metrics::MetricsCollector;
    /// let m = MetricsCollector::new();
    /// assert_eq!(m.total_tx, 0);
    /// assert_eq!(m.total_collisions, 0);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a transmission by the given node.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::metrics::MetricsCollector;
    /// use theatron::types::NodeId;
    /// let mut m = MetricsCollector::new();
    /// m.record_tx(NodeId(1));
    /// assert_eq!(m.total_tx, 1);
    /// ```
    pub fn record_tx(&mut self, node: NodeId) {
        self.total_tx += 1;
        *self.per_node_tx.entry(node).or_insert(0) += 1;
    }

    /// Record a reception by the given node.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::metrics::MetricsCollector;
    /// use theatron::types::NodeId;
    /// let mut m = MetricsCollector::new();
    /// m.record_rx(NodeId(2));
    /// assert_eq!(m.total_rx, 1);
    /// ```
    pub fn record_rx(&mut self, node: NodeId) {
        self.total_rx += 1;
        *self.per_node_rx.entry(node).or_insert(0) += 1;
    }

    /// Record a collision event.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::metrics::MetricsCollector;
    /// let mut m = MetricsCollector::new();
    /// m.record_collision();
    /// assert_eq!(m.total_collisions, 1);
    /// ```
    pub fn record_collision(&mut self) {
        self.total_collisions += 1;
    }

    /// Record airtime used by a transmission.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::metrics::MetricsCollector;
    /// let mut m = MetricsCollector::new();
    /// m.record_airtime(1_000_000);
    /// assert_eq!(m.total_airtime_us, 1_000_000);
    /// ```
    pub fn record_airtime(&mut self, duration_us: u64) {
        self.total_airtime_us += duration_us;
    }

    /// Return the number of transmissions recorded for a node.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::metrics::MetricsCollector;
    /// use theatron::types::NodeId;
    /// let mut m = MetricsCollector::new();
    /// m.record_tx(NodeId(5));
    /// m.record_tx(NodeId(5));
    /// assert_eq!(m.node_tx_count(NodeId(5)), 2);
    /// assert_eq!(m.node_tx_count(NodeId(99)), 0);
    /// ```
    pub fn node_tx_count(&self, node: NodeId) -> u64 {
        self.per_node_tx.get(&node).copied().unwrap_or(0)
    }

    /// Return the number of receptions recorded for a node.
    ///
    /// # Examples
    ///
    /// ```
    /// use theatron::metrics::MetricsCollector;
    /// use theatron::types::NodeId;
    /// let mut m = MetricsCollector::new();
    /// m.record_rx(NodeId(3));
    /// assert_eq!(m.node_rx_count(NodeId(3)), 1);
    /// assert_eq!(m.node_rx_count(NodeId(4)), 0);
    /// ```
    pub fn node_rx_count(&self, node: NodeId) -> u64 {
        self.per_node_rx.get(&node).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_metrics_are_zero() {
        let m = MetricsCollector::new();
        assert_eq!(m.total_tx, 0);
        assert_eq!(m.total_rx, 0);
        assert_eq!(m.total_collisions, 0);
        assert_eq!(m.total_airtime_us, 0);
    }

    #[test]
    fn record_tx_increments_totals() {
        let mut m = MetricsCollector::new();
        let node = NodeId(1);
        m.record_tx(node);
        m.record_tx(node);
        assert_eq!(m.total_tx, 2);
        assert_eq!(m.node_tx_count(node), 2);
        assert_eq!(m.node_tx_count(NodeId(99)), 0);
    }

    #[test]
    fn record_rx_increments_totals() {
        let mut m = MetricsCollector::new();
        let node = NodeId(2);
        m.record_rx(node);
        assert_eq!(m.total_rx, 1);
        assert_eq!(m.node_rx_count(node), 1);
    }

    #[test]
    fn record_collision_increments() {
        let mut m = MetricsCollector::new();
        m.record_collision();
        m.record_collision();
        assert_eq!(m.total_collisions, 2);
    }

    #[test]
    fn record_airtime_accumulates() {
        let mut m = MetricsCollector::new();
        m.record_airtime(1_000_000);
        m.record_airtime(500_000);
        assert_eq!(m.total_airtime_us, 1_500_000);
    }
}
