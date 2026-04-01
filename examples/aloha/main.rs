//! Pure ALOHA simulation example.
//!
//! Runs three ALOHA nodes at ~0.1 packets/s for 10 seconds and prints
//! aggregate statistics.
//!
//! Run with:
//!   cargo run --example aloha

use theatron::aloha::{AlohaNode, PoissonTraffic, LORA_SF7_DURATION_US};
use theatron::scheduler::Scheduler;
use theatron::types::NodeId;

/// Mean inter-arrival time for ~0.1 pkt/s = 10 s = 10_000_000 µs.
const MEAN_INTER_ARRIVAL_US: u64 = 10_000_000;

/// Backoff window: up to 2× the packet duration.
const BACKOFF_RANGE_US: u64 = LORA_SF7_DURATION_US * 2;

/// Simulation duration: 10 seconds.
const SIM_DURATION_US: u64 = 10_000_000;

fn main() {
    let mut scheduler = Scheduler::new(SIM_DURATION_US);

    // Three ALOHA nodes with different RNG seeds for independence.
    let nodes: [(u32, u64, u64); 3] = [
        (1, 0xDEAD_BEEF_0000_0001, 0xCAFE_BABE_0000_0001),
        (2, 0xDEAD_BEEF_0000_0002, 0xCAFE_BABE_0000_0002),
        (3, 0xDEAD_BEEF_0000_0003, 0xCAFE_BABE_0000_0003),
    ];

    for (id, traffic_seed, backoff_seed) in nodes {
        let traffic = PoissonTraffic::new(MEAN_INTER_ARRIVAL_US, traffic_seed);
        let node = AlohaNode::new(NodeId(id), traffic, BACKOFF_RANGE_US, backoff_seed);
        scheduler.add_node(Box::new(node), Some(0));
    }

    println!(
        "Running Pure ALOHA simulation ({} nodes, {:.1}s)...",
        3,
        SIM_DURATION_US as f64 / 1_000_000.0
    );

    scheduler.run();

    let m = &scheduler.metrics;
    let sim_s = SIM_DURATION_US as f64 / 1_000_000.0;
    let throughput = m.total_rx as f64 / sim_s;

    println!("Simulation complete at t={}us", scheduler.current_time());
    println!("  Total TX:         {}", m.total_tx);
    println!("  Total RX:         {}", m.total_rx);
    println!("  Total collisions: {}", m.total_collisions);
    println!("  Throughput:       {:.4} pkt/s", throughput);
}
