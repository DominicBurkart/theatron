mod file_fragmenter;
mod lorawan_adapter;
mod network_server;
mod periodic_interferer;
mod prng;
mod simulated_radio;

use theatron::scheduler::Scheduler;
use theatron::time::ms_to_sim_time;
use theatron::types::NodeId;

use file_fragmenter::FileFragmenter;
use lorawan_adapter::LoRaWanAdapter;
use network_server::NetworkServer;
use periodic_interferer::PeriodicInterferer;

pub const FILE_SIZE: usize = 5120;
pub const CHUNK_SIZE: usize = 51;
pub const INTERVAL_US: u64 = 0;
pub const SIM_DURATION_MS: u32 = 600_000;

pub const EU868_CHANNELS: [u32; 3] = [868_100_000, 868_300_000, 868_500_000];
pub const INTERFERER_PERIOD_US: u64 = 10_000_000;
pub const INTERFERER_DURATION_US: u64 = 500_000;
pub const INTERFERER_SF: u8 = 7;
pub const SEED: u64 = 0xDEAD_BEEF_1234_5678;

fn main() {
    let sim_duration = ms_to_sim_time(SIM_DURATION_MS);
    let mut scheduler = Scheduler::new(sim_duration);

    let file_data: Vec<u8> = (0..FILE_SIZE).map(|i| (i % 251) as u8).collect();
    let fragmenter = FileFragmenter::new(file_data, CHUNK_SIZE, INTERVAL_US);
    let device = LoRaWanAdapter::new(NodeId(1), fragmenter, SEED);
    let server = NetworkServer::new(NodeId(100));

    scheduler.add_node(Box::new(device), Some(0));
    scheduler.add_node(Box::new(server), None);

    for freq in EU868_CHANNELS {
        let interferer = PeriodicInterferer::new(
            INTERFERER_PERIOD_US,
            INTERFERER_SF,
            freq,
            INTERFERER_DURATION_US,
        );
        scheduler.add_interferer(Box::new(interferer), ms_to_sim_time(10_000));
    }

    println!(
        "Running LoRaWAN file transfer simulation ({FILE_SIZE} bytes, {CHUNK_SIZE}-byte fragments)..."
    );

    scheduler.run();

    let m = &scheduler.metrics;
    println!("Simulation complete at t={}us", scheduler.current_time());
    println!("  Total TX:         {}", m.total_tx);
    println!("  Total RX:         {}", m.total_rx);
    println!("  Collisions:       {}", m.total_collisions);
    println!("  Captures:         {}", m.total_captures);
    println!("  Total airtime:    {}us", m.total_airtime_us);
    println!("  Node 1 TX:        {}", m.node_tx_count(NodeId(1)));
    println!("  Node 100 RX:      {}", m.node_rx_count(NodeId(100)));
}
