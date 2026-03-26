mod aloha_node;

use theatron::scheduler::Scheduler;
use theatron::time::ms_to_sim_time;
use theatron::types::NodeId;

use aloha_node::{AlohaNode, AlohaReceiver, PeriodicTraffic};

/// Simulation parameters.
const NUM_SENDERS: u32 = 5;
const PACKETS_PER_SENDER: usize = 20;
const TX_INTERVAL_US: u64 = 2_000_000; // 2s between packets
const POLL_INTERVAL_US: u64 = 500_000; // 500ms poll interval
const TX_DURATION_US: u64 = 200_000; // 200ms per packet (typical LoRa SF7)
const SF: u8 = 7;
const FREQUENCY: u32 = 868_100_000;
const SIM_DURATION_MS: u32 = 120_000; // 2 minutes

fn main() {
    let sim_duration = ms_to_sim_time(SIM_DURATION_MS);
    let mut scheduler = Scheduler::new(sim_duration);

    // Add a passive receiver
    scheduler.add_node(Box::new(AlohaReceiver::new(NodeId(0))), None);

    // Add ALOHA sender nodes
    for i in 1..=NUM_SENDERS {
        let traffic = PeriodicTraffic::new(
            vec![i as u8; 10], // 10-byte payload tagged with sender id
            TX_INTERVAL_US,
            PACKETS_PER_SENDER,
        );
        let node = AlohaNode::new(
            NodeId(i),
            Box::new(traffic),
            POLL_INTERVAL_US,
            SF,
            FREQUENCY,
            TX_DURATION_US,
        );
        scheduler.add_node(Box::new(node), Some(0));
    }

    println!(
        "Running Pure ALOHA simulation ({NUM_SENDERS} senders, {PACKETS_PER_SENDER} packets each)..."
    );

    scheduler.run();

    let m = &scheduler.metrics;
    let expected_tx = (NUM_SENDERS as u64) * (PACKETS_PER_SENDER as u64);
    let pdr = if m.total_tx > 0 {
        m.total_rx as f64 / (m.total_tx as f64 * (NUM_SENDERS as f64))
    } else {
        0.0
    };

    println!("Simulation complete at t={}us", scheduler.current_time());
    println!("  Senders:          {NUM_SENDERS}");
    println!("  Expected TX:      {expected_tx}");
    println!("  Total TX:         {}", m.total_tx);
    println!("  Total RX:         {}", m.total_rx);
    println!("  Collisions:       {}", m.total_collisions);
    println!("  Captures:         {}", m.total_captures);
    println!("  Total airtime:    {}us", m.total_airtime_us);
    println!("  Approx PDR:       {pdr:.2}");

    for i in 1..=NUM_SENDERS {
        println!(
            "  Node {} TX: {}  RX: {}",
            i,
            m.node_tx_count(NodeId(i)),
            m.node_rx_count(NodeId(i))
        );
    }
    println!("  Receiver RX:      {}", m.node_rx_count(NodeId(0)));
}
